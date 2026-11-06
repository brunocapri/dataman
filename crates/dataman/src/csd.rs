//! The window frame Linux leaves to the app (client-side decorations): window buttons in the
//! title bar, thin resize edges along the border, and a menu in place of the macOS menu bar.
//! The frame adds no inset: the app's layout keeps the whole window. On Windows GPUI hides the
//! title bar and keeps the resize border and shadow: the buttons and the menu are ours there too.

use gpui::{
    AnyElement, App, Context, CursorStyle, Decorations, MouseButton, ResizeEdge, Tiling, Window, WindowButton, WindowButtonLayout, div,
    prelude::*,
};

use crate::app::Workspace;
use crate::assets::Icon;
use crate::keymap::NOT_MAC;
use crate::theme::{ActiveTheme, Theme, px};
use crate::ui;

pub const BUTTON: f32 = 28.;
const EDGE: f32 = 4.;
const CORNER: f32 = 10.;

/// Our own window buttons are drawn: Linux with client-side decorations, Windows; not full screen.
pub fn drawn(window: &Window) -> bool {
    !window.is_fullscreen() && (cfg!(windows) || NOT_MAC && matches!(window.window_decorations(), Decorations::Client { .. }))
}

/// The resize edges and the hairline too (Windows keeps its own border).
fn edges_drawn(window: &Window) -> bool {
    !cfg!(windows) && drawn(window)
}

/// GPUI's own `window_handle` is another thing: raw-window-handle's, by its trait.
#[cfg(windows)]
fn hwnd(window: &Window) -> Option<windows_sys::Win32::Foundation::HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(w) => Some(w.hwnd.get() as _),
        _ => None,
    }
}

/// The title bar starts moving the window: Windows' own move loop there (GPUI's is a no-op).
/// Posted, not sent: the loop is modal and would run inside this mouse event, with the app still
/// borrowed, so a key pressed mid-drag (Esc's drag-cancel interceptor) would borrow it again.
pub fn start_move(window: &Window) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
        use windows_sys::Win32::UI::WindowsAndMessaging::{HTCAPTION, PostMessageW, WM_NCLBUTTONDOWN};
        if let Some(hwnd) = hwnd(window) {
            unsafe {
                ReleaseCapture();
                PostMessageW(hwnd, WM_NCLBUTTONDOWN, HTCAPTION as usize, 0);
            }
        }
    }
    #[cfg(not(windows))]
    window.start_window_move();
}

/// GPUI redraws the whole window for every `WM_MOVE`, up to the mouse's rate, though a window that
/// moves shows the same picture: on Windows that makes the compositor miss frames (DAT-33). Inside
/// Windows' move loop the message waits here, and GPUI learns the last position as the loop ends.
#[cfg(windows)]
pub fn quiet_moves(window: &Window) {
    use std::cell::Cell;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, GWLP_WNDPROC, SetWindowLongPtrW, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_MOVE, WM_NCDESTROY, WM_SIZE, WNDPROC,
    };

    thread_local! {
        static NEXT: Cell<WNDPROC> = const { Cell::new(None) };
        static LOOP: Cell<bool> = const { Cell::new(false) };
        static HELD: Cell<Option<LPARAM>> = const { Cell::new(None) };
    }

    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let next = NEXT.get();
        // SAFETY: `next` is the window procedure this one replaced, called as it would have been.
        let pass = |msg, wparam, lparam| unsafe { CallWindowProcW(next, hwnd, msg, wparam, lparam) };
        match msg {
            WM_ENTERSIZEMOVE => LOOP.set(true),
            WM_MOVE if LOOP.get() => {
                HELD.set(Some(lparam));
                return 0;
            }
            // Resizing by a left or top edge moves the window too, and redraws anyway.
            WM_SIZE => {
                if let Some(held) = HELD.take() {
                    pass(WM_MOVE, 0, held);
                }
            }
            WM_EXITSIZEMOVE => {
                LOOP.set(false);
                if let Some(held) = HELD.take() {
                    pass(WM_MOVE, 0, held);
                }
            }
            WM_NCDESTROY => {
                // SAFETY: the window is alive during this message; the procedure goes back first.
                unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, next.map_or(0, |f| f as usize as isize)) };
            }
            _ => {}
        }
        pass(msg, wparam, lparam)
    }

    if let Some(hwnd) = hwnd(window) {
        // SAFETY: on the thread that owns the window; `proc` has the WNDPROC signature, and what
        // SetWindowLongPtrW returns is the procedure it replaced (GPUI's, never null).
        unsafe {
            let old = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, proc as *const () as usize as isize);
            NEXT.set(std::mem::transmute::<isize, WNDPROC>(old));
        }
    }
}

/// A move inside the loop leaves GPUI's position alone, and the loop's end delivers it. The
/// messages are sent outside any `cx.update`: GPUI's move callback borrows the app.
#[cfg(all(windows, feature = "checks"))]
pub async fn check_quiet_moves(cx: &mut gpui::AsyncWindowContext) -> Result<(), String> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowRect, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SendMessageW, SetWindowPos, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE};

    let origin = |cx: &mut gpui::AsyncWindowContext| cx.update(|window, _| window.bounds().origin).map_err(|e| e.to_string());
    let (hwnd, scale) = cx.update(|window, _| (hwnd(window), window.scale_factor())).map_err(|e| e.to_string())?;
    let hwnd = hwnd.ok_or("no window handle")?;
    let from = origin(cx)?;
    const DX: i32 = 60;
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: a live window on its own thread; the message is sent with no pointers in it.
    let moved = unsafe {
        GetWindowRect(hwnd, &mut r);
        SendMessageW(hwnd, WM_ENTERSIZEMOVE, 0, 0);
        SetWindowPos(hwnd, std::ptr::null_mut(), r.left + DX, r.top, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE) != 0
    };
    let held = origin(cx)?;
    // SAFETY: as above.
    unsafe { SendMessageW(hwnd, WM_EXITSIZEMOVE, 0, 0) };
    let after = origin(cx)?;
    // SAFETY: as above.
    unsafe { SetWindowPos(hwnd, std::ptr::null_mut(), r.left, r.top, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE) };
    let want = f32::from(from.x) + DX as f32 / scale;
    if moved && held == from && (f32::from(after.x) - want).abs() < 1. {
        Ok(())
    } else {
        Err(format!("moved {moved}, from {from:?}, during the loop {held:?}, after it {after:?}, wanted x {want}"))
    }
}

/// The maximize button and a double click on the title bar: maximize, or restore a maximized
/// window. GPUI's Windows `zoom` only ever maximizes, so the restore is ours there.
pub fn toggle_maximize(window: &Window) {
    #[cfg(windows)]
    if window.is_maximized() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SW_RESTORE, ShowWindowAsync};
        if let Some(hwnd) = hwnd(window) {
            unsafe { ShowWindowAsync(hwnd, SW_RESTORE) };
        }
        return;
    }
    window.zoom_window();
}

fn tiling(window: &Window) -> Tiling {
    match window.window_decorations() {
        Decorations::Client { tiling } => tiling,
        Decorations::Server => Tiling::tiled(),
    }
}

fn layout(cx: &App) -> WindowButtonLayout {
    let all = [Some(WindowButton::Minimize), Some(WindowButton::Maximize), Some(WindowButton::Close)];
    cx.button_layout().unwrap_or(WindowButtonLayout { left: [None; 3], right: all })
}

fn buttons(side: [Option<WindowButton>; 3], window: &Window) -> Vec<WindowButton> {
    let c = window.window_controls();
    side.into_iter()
        .flatten()
        .filter(|b| match b {
            WindowButton::Minimize => c.minimize,
            WindowButton::Maximize => c.maximize && window.is_resizable(),
            WindowButton::Close => true,
        })
        .collect()
}

/// The title bar's room before its first control: the traffic lights on macOS, the window
/// buttons the desktop puts on the left on Linux.
pub fn lead(window: &Window, cx: &App) -> f32 {
    if !NOT_MAC {
        return crate::theme::metrics::lights_right_at(crate::theme::zoom());
    }
    if !drawn(window) {
        return 0.;
    }
    let n = buttons(layout(cx).left, window).len();
    if n == 0 { 0. } else { n as f32 * BUTTON + 8. }
}

fn button(t: &Theme, b: WindowButton, window: &Window) -> AnyElement {
    let i = match b {
        WindowButton::Minimize => Icon::WinMinimize,
        WindowButton::Maximize if window.is_maximized() => Icon::WinRestore,
        WindowButton::Maximize => Icon::WinMaximize,
        WindowButton::Close => Icon::Close,
    };
    ui::icon_button(t, gpui::SharedString::from(format!("window-{}", b.id())), i, 24., 14.)
        .rounded_full()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, window, cx| match b {
            WindowButton::Minimize => window.minimize_window(),
            WindowButton::Maximize => toggle_maximize(window),
            WindowButton::Close => window.dispatch_action(Box::new(crate::app::Quit), cx),
        })
        .into_any_element()
}

fn group(t: &Theme, side: [Option<WindowButton>; 3], window: &Window) -> Option<AnyElement> {
    let bs = buttons(side, window);
    (!bs.is_empty()).then(|| {
        div()
            .flex_none()
            .h_full()
            .flex()
            .flex_row()
            .items_center()
            .children(bs.into_iter().map(|b| div().w(px(BUTTON)).flex().justify_center().child(button(t, b, window))))
            .into_any_element()
    })
}

impl Workspace {
    pub(crate) fn render_window_buttons_left(&self, window: &Window, cx: &App) -> Option<AnyElement> {
        if !drawn(window) {
            return None;
        }
        let t = cx.theme();
        group(&t, layout(cx).left, window).map(|g| div().pl(px(4.)).h_full().flex_none().child(g).into_any_element())
    }

    pub(crate) fn render_window_buttons_right(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !NOT_MAC {
            return None;
        }
        let t = cx.theme();
        let menu = ui::icon_button(&t, "app-menu", Icon::Menu, 24., 14.)
            .rounded_full()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|w, ev: &gpui::ClickEvent, _, cx| {
                if w.app_menu.is_none() && ui::menu_just_shut("app-menu") {
                    return;
                }
                w.app_menu = if w.app_menu.is_some() { None } else { Some(ev.position()) };
                cx.notify();
            }));
        let right = drawn(window).then(|| group(&t, layout(cx).right, window)).flatten();
        Some(
            div()
                .flex_none()
                .h_full()
                .flex()
                .flex_row()
                .items_center()
                .pr(px(6.))
                .child(div().w(px(BUTTON)).flex().justify_center().child(menu))
                .children(right)
                .into_any_element(),
        )
    }

    /// What the macOS menu bar's Dataman menu holds, plus every command through the palette.
    pub(crate) fn render_app_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let at = self.app_menu?;
        let t = cx.theme();
        let item = |id: &'static str, label: gpui::SharedString, key: Option<String>, action: Box<dyn gpui::Action>| {
            ui::menu_item(&t, id, label, key.as_deref(), true).on_click(cx.listener(move |w, _, window, cx| {
                w.app_menu = None;
                window.dispatch_action(action.boxed_clone(), cx);
                cx.notify();
            }))
        };
        let mut menu = ui::popover(&t)
            .w(px(240.))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|w, _, _, cx| {
                w.app_menu = None;
                ui::menu_shut("app-menu");
                cx.notify();
            }));
        if let Some(label) = crate::updates::menu_label(cx) {
            menu = menu.child(item("am-update", label, None, Box::new(crate::app::ShowUpdate)));
        }
        if crate::updates::enabled(cx) {
            let checking = crate::updates::state(cx) == crate::updates::State::Checking;
            let label = if checking { "Checking for Updates…" } else { "Check for Updates…" };
            menu = menu.child(item("am-check", label.into(), None, Box::new(crate::app::CheckForUpdates))).child(ui::menu_sep(&t));
        }
        menu = menu
            .child(item("am-palette", "All Commands…".into(), crate::keymap::key("command_palette"), Box::new(crate::app::CommandPalette)))
            .child(item("am-settings", "Settings…".into(), crate::keymap::key("settings"), Box::new(crate::app::OpenSettings)))
            .child(ui::menu_sep(&t))
            .child(item("am-quit", "Quit Dataman".into(), crate::keymap::key("quit"), Box::new(crate::app::Quit)));
        let at = gpui::point(at.x - px(240.), at.y + px(14.));
        Some(gpui::deferred(gpui::anchored().position(at).snap_to_window_with_margin(px(8.)).child(ui::menu_in("app-menu-in", menu))).priority(3).into_any_element())
    }

    /// Resize handles along the border (none on a tiled edge).
    pub(crate) fn render_resize_edges(&self, window: &Window) -> Vec<AnyElement> {
        if !edges_drawn(window) || !window.is_resizable() || window.is_maximized() {
            return Vec::new();
        }
        let tl = tiling(window);
        let mut out = Vec::new();
        let mut edge = |e: ResizeEdge, cursor: CursorStyle, place: fn(gpui::Div) -> gpui::Div| {
            out.push(
                place(div().absolute())
                    .id(("resize", e as usize))
                    .cursor(cursor)
                    .occlude()
                    .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                        cx.stop_propagation();
                        window.start_window_resize(e);
                    })
                    .into_any_element(),
            );
        };
        if !tl.top {
            edge(ResizeEdge::Top, CursorStyle::ResizeUpDown, |d| d.top_0().left(px(CORNER)).right(px(CORNER)).h(px(EDGE)));
        }
        if !tl.bottom {
            edge(ResizeEdge::Bottom, CursorStyle::ResizeUpDown, |d| d.bottom_0().left(px(CORNER)).right(px(CORNER)).h(px(EDGE)));
        }
        if !tl.left {
            edge(ResizeEdge::Left, CursorStyle::ResizeLeftRight, |d| d.left_0().top(px(CORNER)).bottom(px(CORNER)).w(px(EDGE)));
        }
        if !tl.right {
            edge(ResizeEdge::Right, CursorStyle::ResizeLeftRight, |d| d.right_0().top(px(CORNER)).bottom(px(CORNER)).w(px(EDGE)));
        }
        if !tl.top && !tl.left {
            edge(ResizeEdge::TopLeft, CursorStyle::ResizeUpLeftDownRight, |d| d.top_0().left_0().size(px(CORNER)));
        }
        if !tl.top && !tl.right {
            edge(ResizeEdge::TopRight, CursorStyle::ResizeUpRightDownLeft, |d| d.top_0().right_0().size(px(CORNER)));
        }
        if !tl.bottom && !tl.left {
            edge(ResizeEdge::BottomLeft, CursorStyle::ResizeUpRightDownLeft, |d| d.bottom_0().left_0().size(px(CORNER)));
        }
        if !tl.bottom && !tl.right {
            edge(ResizeEdge::BottomRight, CursorStyle::ResizeUpLeftDownRight, |d| d.bottom_0().right_0().size(px(CORNER)));
        }
        out
    }
}

/// The frame's hairline on a window that floats (a tiled or maximized one meets its
/// neighbours edge to edge).
pub fn border(window: &Window, cx: &App) -> Option<AnyElement> {
    if !edges_drawn(window) || window.is_maximized() || tiling(window).is_tiled() {
        return None;
    }
    let t = cx.theme();
    Some(div().absolute().top_0().left_0().right_0().bottom_0().border_1().border_color(t.line).into_any_element())
}
