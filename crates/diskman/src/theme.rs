//! The design tokens ("Memory Card" design language): colors for the dark and the light theme,
//! fonts and fixed metrics. The active theme lives in a GPUI global; every view reads it with
//! `cx.theme()` while rendering, so switching themes is one `refresh_windows`.
//!
//! Token names follow the design boards (`Tokens.dc.html` and the `Ui*` palettes):
//! surfaces `win glass bg panel card pop field`, interaction `hov prs`, lines `line line2`, text
//! `tx..tx4`, roles `acc` (you: focus, selection, run), `lar` (LAR and only LAR), states
//! `ok warn err info` with `*_s` soft backgrounds, grid `null null_b sel alt`, keycaps
//! `key key_b`, syntax colors and the connection swatches.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicU8, AtomicU32, Ordering};

use gpui::{App, BoxShadow, Global, Hsla, Pixels, Rgba, Window, WindowAppearance, WindowBackgroundAppearance, point};
use diskman_core::{ThemePref, Translucency, UiFont};

/// The UI zoom (View ▸ Zoom In / Zoom Out / Actual Size, ⌘= ⌘- ⌘0), as `f32` bits: every
/// size in the UI is written in points at 100 % and goes through [`px`], which scales it, so
/// the whole window (explorer, tabs, editor, grid, menus, dialogs) zooms together and text is
/// shaped at its zoomed size (crisp, not a scaled bitmap). An atomic for the same reason as
/// `FONT`: painting code without a context reads it.
static ZOOM: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0

/// The zoom steps, as in browsers and Zed.
pub const ZOOM_STEPS: [f32; 8] = [0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0];

/// The UI zoom factor (1.0 = 100 %).
pub fn zoom() -> f32 {
    f32::from_bits(ZOOM.load(Ordering::Relaxed))
}

/// A UI size in points at 100 %, zoomed: what every view uses instead of `gpui::px`. Window
/// sizes, mouse positions and layout bounds from GPUI are already zoomed; [`Pt::pt`] turns
/// them back into these points.
#[inline]
pub fn px(v: f32) -> Pixels {
    gpui::px(v * zoom())
}

/// GPUI's window coordinates back in the UI's points at 100 % (the inverse of [`px`]).
pub trait Pt {
    fn pt(self) -> f32;
}

impl Pt for Pixels {
    #[inline]
    fn pt(self) -> f32 {
        self.as_f32() / zoom()
    }
}

/// The step after (`dir` > 0) or before `z`, or 1.0 for `dir` = 0; stays at the ends.
pub fn zoom_step(z: f32, dir: i32) -> f32 {
    match dir.signum() {
        0 => 1.0,
        1 => ZOOM_STEPS.iter().copied().find(|s| *s > z + 1e-3).unwrap_or(ZOOM_STEPS[ZOOM_STEPS.len() - 1]),
        _ => ZOOM_STEPS.iter().rev().copied().find(|s| *s < z - 1e-3).unwrap_or(ZOOM_STEPS[0]),
    }
}

/// Sets the zoom without saving it (startup, checks).
pub fn init_zoom(z: f32) {
    ZOOM.store(diskman_core::settings::clamp_zoom(z).to_bits(), Ordering::Relaxed);
}

/// Persists a changed zoom (installed by `main`, which owns the settings file).
pub struct SaveZoom(pub Box<dyn Fn(f32)>);

impl Global for SaveZoom {}

/// Zooms the UI live: stores and saves the factor, moves the traffic lights to the middle of
/// the zoomed title bar and re-renders. Widths kept in points (explorer, side panel, grid
/// columns) scale with it.
pub fn set_zoom(z: f32, window: &mut Window, cx: &mut App) {
    init_zoom(z);
    if let Some(save) = cx.try_global::<SaveZoom>() {
        (save.0)(zoom());
    }
    let (x, y) = metrics::traffic_light_position_at(zoom());
    window.set_traffic_light_position(point(gpui::px(x), gpui::px(y)));
    cx.refresh_windows();
}

/// The pixel face: logotype, empty-state tags and the LAR badge only.
pub const PIXEL: &str = "DotGothic16";

/// The UI font (`UiFont::ALL` index). An atomic rather than a global: the grid and the editor
/// read it while shaping, often from free functions without a context, and a relaxed load is
/// all it costs.
static FONT: AtomicU8 = AtomicU8::new(0);
/// The families already handed to the text system (bit `i` = `UiFont::ALL[i]`).
static REGISTERED: AtomicU8 = AtomicU8::new(0);
/// An installed system family in use instead of one of the four embedded ones (Settings ▸
/// Appearance ▸ Font ▸ a name typed below them); null when none is active. Leaked rather than
/// kept in a `Mutex`, for the same reason as `FONT`: changing it is rare (a Settings click),
/// reading it (every shape) must stay a relaxed load.
static SYSTEM_FONT: AtomicPtr<&'static str> = AtomicPtr::new(std::ptr::null_mut());

/// The UI font (Settings ▸ Appearance ▸ Font).
pub fn ui_font() -> UiFont {
    UiFont::ALL.get(FONT.load(Ordering::Relaxed) as usize).copied().unwrap_or_default()
}

/// The installed system family in use, if any (Settings ▸ Appearance ▸ Font).
pub fn system_font() -> Option<&'static str> {
    let p = SYSTEM_FONT.load(Ordering::Relaxed);
    (!p.is_null()).then(|| unsafe { *p })
}

/// Monospace everywhere: UI, editor and grid: the installed system family in use, else one of
/// the four embedded ones.
pub fn mono() -> &'static str {
    system_font().unwrap_or_else(|| ui_font().family())
}

/// The chosen family's advance in em: 0.6 for all but Zed Mono's narrow Iosevka cut (0.5).
/// For estimates made without shaping (explorer meta, plan labels, live grid columns).
pub fn advance() -> f32 {
    if ui_font() == UiFont::ZedMono { 0.5 } else { 0.6 }
}

/// A family's five faces (Regular, Medium, SemiBold, Bold, Italic). The three added families
/// are subsets (Latin, Greek, Cyrillic, punctuation, arrows, math, box drawing, shapes; no
/// hinting; no `calt`/`liga`/`dlig`: data shows literally); what they lack falls back to the
/// system through CoreText. All OFL; the licenses are next to them in `assets/fonts`.
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

/// Startup: the chosen family and the pixel face. The other families register when first
/// needed (`ensure_fonts`), so they cost nothing at launch.
pub fn register_fonts(font: UiFont, cx: &App) {
    FONT.store(UiFont::ALL.iter().position(|f| *f == font).unwrap_or(0) as u8, Ordering::Relaxed);
    let pixel: Cow<'static, [u8]> = Cow::Borrowed(include_bytes!("../assets/fonts/DotGothic16-Regular.ttf"));
    add_families(&[font], Some(pixel), cx);
}

/// Registers the families not registered yet (the Settings row shows each name in its face).
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
    // `add_fonts` also drops the text system's resolved fonts, so nothing keeps a fallback
    // picked before the family was there.
    match cx.text_system().add_fonts(bytes) {
        Ok(()) => {
            REGISTERED.fetch_or(bits, Ordering::Relaxed);
        }
        Err(e) => log::warn!("could not register the embedded fonts: {e:#}"),
    }
}

/// Persists a changed UI font (installed by `main`, which owns the settings file).
pub struct SaveFont(pub Box<dyn Fn(UiFont)>);

impl Global for SaveFont {}

/// Switches the UI font live (Settings, palette): registers it, saves it, and re-renders every
/// window. Views measure text while rendering (the grid's character width, the editor's
/// advance and gutter), so the next frame lays out in the new face; the grid rescales column
/// widths made for the old one.
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

/// Persists a changed system font choice (installed by `main`, which owns the settings file).
pub struct SaveSystemFont(pub Box<dyn Fn(Option<String>)>);

impl Global for SaveSystemFont {}

fn leak_font_name(name: &str) -> *mut &'static str {
    Box::into_raw(Box::new(Box::leak(name.to_string().into_boxed_str()) as &'static str))
}

/// Switches to an installed system family (Settings ▸ Appearance ▸ Font): one of the four
/// embedded ones goes back with `set_font`. `name` is leaked into `SYSTEM_FONT` rather than
/// kept behind a lock, for the same reason `FONT` is an atomic: changing it is as rare as a
/// Settings click, reading it (every shape) must stay a relaxed load — a few bytes per change
/// is not worth the lock.
pub fn set_system_font(name: Option<String>, cx: &mut App) {
    let p = name.as_deref().map(leak_font_name).unwrap_or(std::ptr::null_mut());
    SYSTEM_FONT.store(p, Ordering::Relaxed);
    if let Some(save) = cx.try_global::<SaveSystemFont>() {
        (save.0)(name);
    }
    cx.refresh_windows();
}

/// Startup: activates a system font saved in `settings.toml` only if it is still installed — a
/// family removed since then falls back to the embedded choice quietly, as if none had ever
/// been picked.
pub fn activate_system_font(name: &str, cx: &App) {
    if cx.text_system().all_font_names().iter().any(|f| f == name) {
        SYSTEM_FONT.store(leak_font_name(name), Ordering::Relaxed);
    }
}

/// Fixed metrics, in points (Spec board, reference window 1400 x 900).
#[allow(dead_code)]
pub mod metrics {
    pub const TITLE_H: f32 = 40.;
    pub const TAB_H: f32 = 28.;
    pub const TOOLBAR_H: f32 = 40.;
    pub const RESULT_BAR_H: f32 = 34.;
    /// The result tabs of a script, over the result bar.
    pub const STRIP_H: f32 = 30.;
    pub const STATUS_H: f32 = 28.;
    pub const EXPLORER_W: f32 = 260.;
    pub const EXPLORER_MIN: f32 = 200.;
    pub const EXPLORER_MAX: f32 = 480.;
    /// Explorer width when the window opens at the compact size (1280 x 800 or less).
    pub const EXPLORER_COMPACT: f32 = 220.;
    pub const SIDE_W: f32 = 360.;
    pub const SIDE_MIN: f32 = 280.;
    pub const SIDE_MAX: f32 = 520.;
    pub const EDITOR_H: f32 = 300.;
    pub const EDITOR_COMPACT: f32 = 240.;
    pub const EDITOR_MIN: f32 = 80.;
    pub const GRID_MIN: f32 = 120.;
    pub const DIVIDER: f32 = 7.;
    /// Hit area of a divider (2 pt past each side of the visible bar).
    pub const DIVIDER_HIT: f32 = 11.;
    pub const TREE_ROW: f32 = 22.;
    pub const GRID_ROW: f32 = 22.;
    pub const GRID_HEADER: f32 = 28.;
    pub const EDITOR_LINE: f32 = 20.;
    pub const MIN_WINDOW: (f32, f32) = (960., 600.);

    /// The traffic lights as drawn: 12 pt circles, 20 pt apart (8 pt gaps), the first one's
    /// top-left at (16, 14). Their centres sit on the title bar's middle line
    /// (14 + 12 / 2 = 20 = TITLE_H / 2), where the tabs, the explorer button and the ⌘K hint
    /// are centred too.
    pub const LIGHTS_AT: (f32, f32) = (16., 14.);
    pub const LIGHT_D: f32 = 12.;
    pub const LIGHT_STEP: f32 = 20.;
    /// AppKit's standard window button is a 14 x 16 frame with its 12 pt circle centred in it
    /// (1 pt in from the left, 2 pt down from the top; measured on macOS 15), and GPUI's
    /// `traffic_light_position` places that frame's origin: it sizes the title bar container
    /// to the button's height plus twice the y, so the y is the frame's distance from the
    /// window's top whichever way the container is flipped.
    pub const LIGHT_FRAME: (f32, f32) = (14., 16.);

    /// GPUI's `traffic_light_position` for circles at `LIGHTS_AT`: (15, 12).
    pub fn traffic_light_position() -> (f32, f32) {
        (LIGHTS_AT.0 - (LIGHT_FRAME.0 - LIGHT_D) / 2., LIGHTS_AT.1 - (LIGHT_FRAME.1 - LIGHT_D) / 2.)
    }

    /// The same at a UI zoom: AppKit's lights keep their size and x; their centre stays on the
    /// zoomed title bar's middle line.
    pub fn traffic_light_position_at(zoom: f32) -> (f32, f32) {
        let (x, _) = traffic_light_position();
        (x, (TITLE_H * zoom - LIGHT_FRAME.1) / 2.)
    }

    /// Right edge of the zoom light's circle in the UI's points at `zoom` (the lights do not
    /// scale: at 200 % they take half as many points).
    pub fn lights_right_at(zoom: f32) -> f32 {
        LIGHTS_RIGHT / zoom
    }

    /// Centre of light `i` (0 close, 1 minimize, 2 zoom), from the window's top-left.
    pub fn light_centre(i: usize) -> (f32, f32) {
        let (x, y) = traffic_light_position();
        (x + i as f32 * LIGHT_STEP + LIGHT_FRAME.0 / 2., y + LIGHT_FRAME.1 / 2.)
    }

    /// Right edge of the zoom light's circle (the explorer button starts 14 pt after it).
    pub const LIGHTS_RIGHT: f32 = LIGHTS_AT.0 + 2. * LIGHT_STEP + LIGHT_D;

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_title_bar_centres_on_the_traffic_lights() {
            assert_eq!(traffic_light_position(), (15., 12.));
            let mid = TITLE_H / 2.;
            for i in 0..3 {
                assert_eq!(light_centre(i).1, mid);
            }
            assert_eq!(light_centre(0).0, LIGHTS_AT.0 + LIGHT_D / 2.);
            assert_eq!(LIGHTS_RIGHT, 68.);
            // Query tabs (28 pt) and the explorer button (26 pt) are centred in the bar.
            assert_eq!((TITLE_H - TAB_H) / 2. + TAB_H / 2., mid);
            assert_eq!((TITLE_H - 26.) / 2. + 13., mid);
            // Zoomed, the lights' centre follows the bar's middle line.
            assert_eq!(traffic_light_position_at(1.), traffic_light_position());
            for z in [0.8, 1.25, 2.] {
                let (_, y) = traffic_light_position_at(z);
                assert!((y + LIGHT_FRAME.1 / 2. - TITLE_H * z / 2.).abs() < 1e-4);
            }
        }
    }
}

/// Type scale (sizes in points).
#[allow(dead_code)]
pub mod text {
    pub const CODE: f32 = 13.;
    pub const BODY: f32 = 12.;
    pub const META: f32 = 11.;
    pub const LABEL: f32 = 10.5;
    pub const TITLE: f32 = 13.5;
    pub const MESSAGE: f32 = 18.;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
}

/// A connection color swatch (UiConnDialog), by name as stored in the profile.
pub const SWATCHES: [&str; 8] = ["rose", "coral", "lime", "mint", "sky", "cobalt", "sand", "slate"];

/// Every token of the boards is here, including the ones only later phases use.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Theme {
    pub mode: Mode,
    // Surfaces.
    /// The shell surface, solid (Zeron's `surface`, zeronsh/zeron, MIT).
    pub win: Hsla,
    /// The window's tint over the system blur, painted once behind the whole window (title
    /// bar, explorer, status bar, and under the work surface): `win` at 86 % (`GLASS_ALPHA`).
    /// Solid `win` when the window is opaque.
    pub glass: Hsla,
    /// The main panel's base, solid (Zeron's `bg`): one step below the shell in the dark
    /// theme, white in the light one. Fills that must hide what is under them (the grid's cell
    /// editor) and the wells of opaque floating surfaces (dialog footers).
    pub bg: Hsla,
    /// The work column's fill (query tabs, editor, grid, side panel, welcome),
    /// solid `bg` when the window is opaque. The work column paints it ONCE, flush from the
    /// window's top edge to the status bar and from the seam to the right edge; the regions inside it
    /// are transparent and separated by the `line`/`line2` hairlines (a second coat would
    /// darken the overlap).
    ///
    /// GPUI blends color `src·a + dst·(1−a)` but adds alpha (`a + dst_a`). Dark: `bg` at 7 %
    /// over the 0.86 glass lands at alpha 0.93, so 7 % of the blurred desktop still shows
    /// through, one step darker than the explorer (Zeron's 40 % would reach alpha 1 and hide
    /// the blur). Light: solid white — with this blending any translucent white panel ends up
    /// darker than the glass over a light desktop, the opposite of Zeron's white panel in a
    /// grey shell. Overlays inside the surface (selection, cards) add alpha too and are opaque.
    pub panel: Hsla,
    /// A raised card inside the surface (viewer body, error excerpt, live log, state cards):
    /// Zeron's `card_glass_bg()`, `win` at 40 %; solid `win` when opaque.
    pub card: Hsla,
    /// Floating surfaces (menus, popovers, dialogs, palette): opaque. Zeron frosts them with
    /// an in-app backdrop blur that our GPUI does not have; translucent without the blur the
    /// text under them would show through.
    pub pop: Hsla,
    pub field: Hsla,
    /// A text field's border (filters, dialog fields, find bars): opaque, so it holds over any
    /// desktop (a translucent white outline on the dark glass cancels out over a white one).
    pub field_b: Hsla,
    // Interaction.
    pub hov: Hsla,
    pub prs: Hsla,
    /// The editor's current line (a little fainter than `hov`).
    pub cur_line: Hsla,
    // Lines. Opaque: a translucent hairline over the glass or the translucent surface would let
    // the desktop through and lose its contrast.
    pub line: Hsla,
    pub line2: Hsla,
    /// Grid cell borders (slightly fainter than `line`).
    pub grid_line: Hsla,
    /// Outline of a ROUNDED shape sitting on the glass (the status bar's badge, the explorer's
    /// selected row; not text fields, see `field_b`). Translucent on purpose: GPUI adds alpha, so an
    /// opaque color on the antialiased pixels of a curve saturates them to alpha 1 with the
    /// glass's premultiplied color — a dark fringe over a light desktop. Kept at ≤ 0.2 alpha,
    /// what the glass leaves before 1. Straight, pixel-aligned separators use `line`.
    pub edge: Hsla,
    /// `edge` in the accent (focused field, focused selection on the glass).
    pub edge_acc: Hsla,
    // Text.
    pub tx: Hsla,
    pub tx2: Hsla,
    pub tx3: Hsla,
    pub tx4: Hsla,
    // You: focus, selection, run.
    pub acc: Hsla,
    pub acc_s: Hsla,
    pub acc_t: Hsla,
    pub acc_h: Hsla,
    pub acc_p: Hsla,
    pub on_acc: Hsla,
    // LAR, and only LAR.
    pub lar: Hsla,
    pub lar_s: Hsla,
    /// The ring of the toolbar's LAR group.
    pub lar_r: Hsla,
    pub lar_t: Hsla,
    pub lar_h: Hsla,
    pub lar_p: Hsla,
    pub on_lar: Hsla,
    // States.
    pub ok: Hsla,
    pub ok_s: Hsla,
    pub warn: Hsla,
    pub warn_s: Hsla,
    pub err: Hsla,
    pub err_s: Hsla,
    /// Background of the erroneous token in the editor.
    pub err_l: Hsla,
    /// Background of the failed statement's lines in the editor.
    pub err_line: Hsla,
    pub on_err: Hsla,
    pub info: Hsla,
    pub info_s: Hsla,
    // Grid.
    pub null: Hsla,
    pub null_b: Hsla,
    /// Selected cells in the focused grid.
    pub sel: Hsla,
    /// The selection at the start of a click's pop (`motion::PopStyle::Select`), fading to `sel`.
    pub sel_hi: Hsla,
    /// Xcode's find-indicator yellow: the box that pops on something found (⌘' on the error,
    /// the row an FK "Go to" lands on, the explorer filter's first match). Opaque.
    pub find: Hsla,
    /// Text drawn on `find` (dark in both themes, like Xcode's).
    pub find_tx: Hsla,
    /// Every match of an open find bar (editor ranges, grid cells): the yellow, faint.
    pub find_s: Hsla,
    /// Every other row.
    pub alt: Hsla,
    /// The rows between them, where the surface still lets the desktop through (dark, on the
    /// glass): GPUI adds a wash's alpha, so a lone white stripe also hides that much more of
    /// the desktop and, over a white one, the two cancel. With the same alpha on every row (dark
    /// here, light there) they differ by `alt.a · 255` levels over any desktop. Transparent
    /// where the surface is already solid.
    pub alt_base: Hsla,
    // Keycaps.
    pub key: Hsla,
    pub key_b: Hsla,
    // Syntax.
    pub kw: Hsla,
    pub func: Hsla,
    pub str: Hsla,
    pub num: Hsla,
    pub typ: Hsla,
    pub cm: Hsla,
    pub op: Hsla,
    /// LAR keywords (`FOR SYSTEM_TIME AS OF`, `SUBSCRIBE`, `LISTEN`, `lar_history`).
    pub lkw: Hsla,
    pub param: Hsla,
    /// Soft backgrounds of the autocomplete kind tags.
    pub kw_s: Hsla,
    pub func_s: Hsla,
    pub typ_s: Hsla,
    pub view_s: Hsla,
    /// Dim layer behind modal sheets.
    pub scrim: Hsla,
    /// Connection swatches, in `SWATCHES` order.
    pub swatches: [Hsla; 8],
}

/// The frost over the system blur, the same in both themes: Zeron's 0.80 raised a step
/// (13/10), so less of the desktop shows where the blur is weak or missing.
pub const GLASS_ALPHA: f32 = 0.86;
/// The dark work surface over the glass: 0.86 + 0.07 = 0.93 total, the desktop showing half as
/// much as through the explorer, as at 0.80 + 0.10 (see [`Theme::panel`]).
pub const PANEL_ALPHA: f32 = 0.07;
/// Zeron's `card_glass_bg()`: the shell surface at 40 %.
pub const CARD_ALPHA: f32 = 0.40;
/// The grid's every-other-row wash (dark, light). On the translucent dark surface the rows
/// between get black at the same alpha (`Theme::alt_base`), so the stripes keep their
/// ~5.6 levels over a white desktop too; the light surface is solid white, where 0.032 of
/// ink is ~8 levels.
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

/// `a` at `k` = 0 to `b` at `k` = 1, straight through RGBA (an HSL path would swing the hue).
pub fn mix(a: Hsla, b: Hsla, k: f32) -> Hsla {
    let (a, b): (Rgba, Rgba) = (a.into(), b.into());
    let k = k.clamp(0., 1.);
    let l = |x: f32, y: f32| x + (y - x) * k;
    Rgba { r: l(a.r, b.r), g: l(a.g, b.g), b: l(a.b, b.b), a: l(a.a, b.a) }.into()
}

impl Theme {
    pub fn dark() -> Theme {
        const W: u32 = 0xffffff;
        // Zeron's dark ladder: panel #060606, shell #0d0d0d, card #0e0e0e, overlay #161616.
        Theme {
            mode: Mode::Dark,
            win: c(0x0d0d0d),
            glass: ca(0x0d0d0d, GLASS_ALPHA),
            bg: c(0x060606),
            panel: ca(0x060606, PANEL_ALPHA),
            card: ca(0x0d0d0d, CARD_ALPHA),
            pop: c(0x161616),
            field: c(0x0e0e0e),
            // Past the dark glass over a white desktop (~#2F2F2F) and over a black one.
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
            // 4.6:1 on the surface over a white desktop (see the contrast test).
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
            lar_p: c(0x8f66f0),
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
            // A touch deeper than Xcode's #fdf05a so it does not glare on the dark surface.
            find: c(0xf5d33f),
            find_tx: c(0x1c1600),
            find_s: ca(0xf5d33f, 0.16),
            alt: ca(W, ALT_GLASS.0),
            alt_base: ca(0x000000, ALT_GLASS.0),
            key: ca(W, 0.06),
            key_b: ca(W, 0.16),
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
            // Rose leans magenta: production's `err` red stays the only red chip.
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
        // Zeron's light ladder: white panel inside a neutral-100 (#f5f5f5) shell.
        Theme {
            mode: Mode::Light,
            win: c(0xf5f5f5),
            glass: ca(0xf5f5f5, GLASS_ALPHA),
            bg: c(0xffffff),
            panel: c(0xffffff),
            card: ca(0xf5f5f5, CARD_ALPHA),
            pop: c(0xffffff),
            field: c(0xffffff),
            // Past the light glass over a black desktop (~#D3D3D3) and white fields.
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
            // 4.6:1 on the surface over a black desktop (see the contrast test).
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
            lar_p: c(0x6a3cf0),
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
            alt: ca(K, ALT_GLASS.1),
            alt_base: gpui::transparent_black(),
            key: ca(K, 0.04),
            key_b: ca(K, 0.18),
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

    /// The same theme for an opaque window (`translucency = "off"` or `DISKMAN_OPAQUE=1`):
    /// the glass, the surface and the cards turn solid.
    pub fn opaque(mut self) -> Theme {
        self.glass = self.glass.alpha(1.);
        self.panel = self.panel.alpha(1.);
        self.card = self.card.alpha(1.);
        self.alt = self.alt.alpha(if self.is_dark() { ALT_OPAQUE.0 } else { ALT_OPAQUE.1 });
        self.alt_base = gpui::transparent_black();
        self
    }

    /// The theme for the window's translucency: as built when on, solid when off.
    pub fn at(self, level: Translucency) -> Theme {
        if level.is_on() { self } else { self.opaque() }
    }

    pub fn is_dark(&self) -> bool {
        self.mode == Mode::Dark
    }

    /// The color of a connection: its saved swatch, or one picked from its id.
    /// A connection's chip: `err` red for production, its swatch otherwise.
    pub fn profile_color(&self, p: &diskman_core::ConnectionProfile) -> Hsla {
        if p.env.is_production() { self.err } else { self.conn_color(p.color.as_deref(), &p.id) }
    }

    pub fn conn_color(&self, swatch: Option<&str>, id: &str) -> Hsla {
        let ix = swatch
            .and_then(|s| SWATCHES.iter().position(|w| *w == s))
            .unwrap_or_else(|| id.bytes().fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize)) % 5);
        self.swatches[ix]
    }

    /// The 1 pt `line2` ring around anything that floats (menus, popovers, toasts, tooltips,
    /// dialogs and sheets), drawn outside it. No drop shadow: a blurred shadow over the glass
    /// (the window is translucent) reads as a dirty halo, so the ring alone separates them.
    pub fn ring(&self) -> Vec<BoxShadow> {
        // A hairline like the borders: 1 pt at any zoom.
        vec![BoxShadow { color: self.line2, offset: point(px(0.), px(0.)), blur_radius: px(0.), spread_radius: gpui::px(1.), inset: false }]
    }

    /// Focus halo (3 pt of `acc_s` around a focused field or region).
    pub fn halo(&self) -> Vec<BoxShadow> {
        vec![BoxShadow { color: self.acc_s, offset: point(px(0.), px(0.)), blur_radius: px(0.), spread_radius: px(3.), inset: false }]
    }
}

/// How translucent the window is (`Off`: opaque, the glass tint drawn solid).
pub struct WindowGlass {
    pub level: Translucency,
}

impl Global for WindowGlass {}

/// Persists a changed preference (installed by `main`, which owns the settings file).
pub struct SavePref(pub Box<dyn Fn(ThemePref)>);

impl Global for SavePref {}

/// The active theme and the user's preference.
pub struct ThemeState {
    pub pref: ThemePref,
    pub theme: Arc<Theme>,
}

impl Global for ThemeState {}

pub trait ActiveTheme {
    /// The active theme (an `Arc`, so callers can keep it across `&mut` borrows).
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

/// Installs the theme for a preference (startup).
pub fn init(pref: ThemePref, cx: &mut App) {
    let mode = resolve(pref, cx.window_appearance());
    let theme = Arc::new(theme_for(mode, cx));
    cx.set_global(ThemeState { pref, theme });
    apply_window_appearance(pref, mode, cx);
}

fn theme_for(mode: Mode, cx: &App) -> Theme {
    let t = match mode {
        Mode::Dark => Theme::dark(),
        Mode::Light => Theme::light(),
    };
    t.at(glass_level(cx))
}

/// The window's translucency (on until `main` decides).
pub fn glass_level(cx: &App) -> Translucency {
    cx.try_global::<WindowGlass>().map_or(Translucency::On, |g| g.level)
}

/// The level the window really uses: `DISKMAN_OPAQUE=1` forces it off.
pub fn effective_level(level: Translucency) -> Translucency {
    if std::env::var_os("DISKMAN_OPAQUE").is_some() { Translucency::Off } else { level }
}

/// The platform background for a level: the system blur (GPUI's, or transparent over Diskman's
/// own blur view where GPUI's does not work: `glass`), or opaque.
pub fn background_for(level: Translucency) -> WindowBackgroundAppearance {
    crate::glass::background_for(level)
}

/// Switches the window's translucency (View menu, Settings ▸ Appearance, palette): saves it,
/// sets the window's background, and the theme's glass, surface and card tokens follow.
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

/// Persists a changed translucency level (installed by `main`, which owns the settings file).
pub struct SaveTranslucency(pub Box<dyn Fn(Translucency)>);

impl Global for SaveTranslucency {}

/// The native chrome (traffic lights, blur material, menus' popovers) follows the theme.
fn apply_window_appearance(pref: ThemePref, mode: Mode, cx: &App) {
    let forced = match (pref, mode) {
        (ThemePref::System, _) => None,
        (_, Mode::Dark) => Some(WindowAppearance::Dark),
        (_, Mode::Light) => Some(WindowAppearance::Light),
    };
    cx.set_window_appearance(forced);
}

/// Changes the preference, saves it, and re-renders every window.
pub fn set_pref(pref: ThemePref, cx: &mut App) {
    let mode = resolve(pref, cx.window_appearance());
    let theme = Arc::new(theme_for(mode, cx));
    cx.set_global(ThemeState { pref, theme });
    apply_window_appearance(pref, mode, cx);
    if let Some(save) = cx.try_global::<SavePref>() {
        (save.0)(pref);
    }
    cx.refresh_windows();
}

/// The next preference in the cycle dark → light → system.
pub fn next_pref(pref: ThemePref) -> ThemePref {
    match pref {
        ThemePref::Dark => ThemePref::Light,
        ThemePref::Light => ThemePref::System,
        ThemePref::System => ThemePref::Dark,
    }
}

/// With the "system" preference, follows a change of the system appearance.
pub fn on_system_appearance(window: &Window, cx: &mut App) {
    let Some(state) = cx.try_global::<ThemeState>() else { return };
    if state.pref != ThemePref::System {
        return;
    }
    let mode = resolve(ThemePref::System, window.appearance());
    if state.theme.mode != mode {
        let theme = Arc::new(theme_for(mode, cx));
        cx.set_global(ThemeState { pref: ThemePref::System, theme });
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

    /// `fg` (maybe translucent) composited over an opaque `bg`.
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

    /// Production's chip is `err` red, so no connection swatch may read as red: each keeps 20°
    /// of hue from `err` (rose sat at 4–5° before), at its usual weight (not louder than `err`).
    #[test]
    fn swatches_stay_clear_of_production_red() {
        for t in [Theme::dark(), Theme::light()] {
            for (name, s) in SWATCHES.iter().zip(t.swatches) {
                if s.s < 0.3 {
                    continue; // sand and slate: no hue to speak of
                }
                let d = (s.h - t.err.h).abs() * 360.;
                let d = d.min(360. - d);
                assert!(d >= 20., "{name} is {d:.0}° from err ({:?})", t.mode);
            }
            let rose = t.swatches[0];
            assert!(rose.s <= t.err.s + 0.05 && (rose.l - t.err.l).abs() < 0.1, "rose at err's weight ({:?})", t.mode);
        }
    }

    /// What GPUI's blending leaves under the surface: premultiplied color with additive alpha
    /// (`theme::Theme::panel`). At alpha 1 the desktop no longer shows; below it, the rest is
    /// the blurred desktop.
    fn gpui_stack(layers: &[Hsla]) -> ([f32; 3], f32) {
        let (mut c, mut a) = ([0f32; 3], 0f32);
        for l in layers {
            let s = rgb(*l);
            c = [0, 1, 2].map(|i| s[i] * l.a + c[i] * (1. - l.a));
            a = (a + l.a).min(1.);
        }
        (c, a)
    }

    /// Zeron's adverse-desktop check (white behind the dark theme, black behind the light
    /// one) on what GPUI really composites: the faintest text that carries information
    /// (`tx3`) keeps 4.5:1 on the surface, on the in-surface cards and on the opaque surface.
    #[test]
    fn muted_text_reads_on_the_translucent_surface() {
        for (t, adverse) in [(Theme::dark(), [1.; 3]), (Theme::light(), [0.; 3])] {
            let (stack, alpha) = gpui_stack(&[t.glass, t.panel]);
            // What the window server shows: the premultiplied stack over the adverse desktop
            // (Zeron's plain flatten would model the panel as normal blending, which GPUI's
            // additive alpha does not do).
            let surface = [0, 1, 2].map(|i| stack[i] + adverse[i] * (1. - alpha));
            // A card adds alpha too and ends opaque over the stack.
            let card = over(t.card, stack);
            match t.mode {
                // 7 % of the blurred desktop still shows through the dark surface.
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

    /// The find indicator: opaque yellow with dark text on it (well past 7:1), and the box
    /// stands out from the surface it pops on; a click's pop starts bluer than the selection.
    #[test]
    fn find_yellow_reads_and_stands_out() {
        for t in [Theme::dark(), Theme::light()] {
            assert_eq!(t.find.a, 1., "{:?} find is opaque (it hides the text under it)", t.mode);
            let r = contrast(rgb(t.find_tx), rgb(t.find));
            assert!(r >= 10., "{:?} find_tx on find: {r:.2}:1", t.mode);
            let surface = if t.is_dark() { rgb(t.bg) } else { [1.; 3] };
            let lift = contrast(rgb(t.find), surface);
            // On white a yellow can only differ so much; on the dark surface it must leap out.
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
        // The ends hold; ⌘0 is 100 %; a value between steps goes to the next one.
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

    /// The grid's zebra rows over a white and a black desktop, as the window server shows
    /// GPUI's stack (premultiplied color, additive alpha clamped at 1): the stripes differ by a
    /// few levels either way (a lone white stripe on the dark glass vanished over white).
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

    /// View ▸ Transparency: on is the theme as built (surface at 0.93 over the glass in the
    /// dark theme), off is solid; the zebra rows are a little stronger when on; the separators
    /// and the fields' borders never let the desktop through.
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
