//! The window's blur when GPUI's does not work.
//!
//! GPUI (`gpui_macos/src/window.rs`, `set_background_appearance`) blurs the desktop with an
//! `NSVisualEffectView` subclass (`BlurredView`: material `.selection`, state active, the
//! default `.behindWindow` blending) added under its Metal view, and then edits that view's
//! private layer tree on every `updateLayer`: it hides the `CAChameleonLayer` (the wallpaper
//! tint), clears every sublayer's background and drops any layer filter whose description
//! contains "Saturat" (the `colorSaturate` filter of macOS ≤ 15). That works on macOS 15
//! (here); on macOS 26 the blur is reported lost — the desktop shows through sharp. The edits
//! match on private class names and filter descriptions, so a material rebuilt in a new
//! release (a blur filter that also carries a saturation parameter, a renamed layer) loses its
//! blur with them; `.selection` is also no window background material.
//!
//! So on macOS 26 and later (which reports itself as 16.x to binaries built with an older
//! SDK) Diskman asks GPUI for a plain transparent window and puts its own stock
//! `NSVisualEffectView` (`.underWindowBackground`, `.behindWindow`, active, its layers left
//! alone) at the bottom of the content view, under GPUI's view; the theme's glass tint is drawn
//! over it as before. `DISKMAN_BLUR=own` / `DISKMAN_BLUR=gpui` picks one on any version.

use gpui::{Window, WindowBackgroundAppearance};
use diskman_core::Translucency;

/// Whose blur the window uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blur {
    /// GPUI's `WindowBackgroundAppearance::Blurred`.
    Gpui,
    /// A stock `NSVisualEffectView` of Diskman's under a transparent GPUI window.
    Own,
}

/// The blur for this system (`DISKMAN_BLUR` overrides).
pub fn blur() -> Blur {
    static B: std::sync::OnceLock<Blur> = std::sync::OnceLock::new();
    *B.get_or_init(|| {
        let b = match std::env::var("DISKMAN_BLUR").ok().as_deref() {
            Some("own") => Blur::Own,
            Some("gpui") => Blur::Gpui,
            _ if macos_major() >= 16 => Blur::Own,
            _ => Blur::Gpui,
        };
        log::info!("window blur: {b:?} (macOS {})", macos_major());
        b
    })
}

/// The platform background for a translucency level.
pub fn background_for(level: Translucency) -> WindowBackgroundAppearance {
    match (level.is_on(), blur()) {
        (false, _) => WindowBackgroundAppearance::Opaque,
        (true, Blur::Gpui) => WindowBackgroundAppearance::Blurred,
        (true, Blur::Own) => WindowBackgroundAppearance::Transparent,
    }
}

/// Sets the window's background for `level`, with Diskman's own blur view under it when that is
/// the blur in use (and none otherwise).
pub fn apply(level: Translucency, window: &mut Window) {
    window.set_background_appearance(background_for(level));
    imp::set_own_view(window, level.is_on() && blur() == Blur::Own);
}

/// Whether Diskman's blur view is in the window now (the checks).
pub fn own_view_installed() -> bool {
    imp::installed()
}

/// Diskman's blur view as AppKit has it (the checks): whether it is the content view's first
/// subview (under GPUI's), and its size against the content view's.
pub fn own_view_layout() -> Option<(bool, (f64, f64), (f64, f64))> {
    imp::layout()
}

#[cfg(target_os = "macos")]
fn macos_major() -> isize {
    imp::os_major()
}

#[cfg(not(target_os = "macos"))]
fn macos_major() -> isize {
    0
}

#[cfg(target_os = "macos")]
mod imp {
    use std::cell::Cell;

    use gpui::Window;
    use objc2::encode::{Encode, Encoding};
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Version {
        major: isize,
        minor: isize,
        patch: isize,
    }

    // SAFETY: the layout of Foundation's `NSOperatingSystemVersion` (three NSIntegers).
    unsafe impl Encode for Version {
        const ENCODING: Encoding = Encoding::Struct("NSOperatingSystemVersion", &[isize::ENCODING, isize::ENCODING, isize::ENCODING]);
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Point {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Size {
        w: f64,
        h: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Rect {
        origin: Point,
        size: Size,
    }

    // SAFETY: CoreGraphics' CGPoint / CGSize / CGRect (doubles on 64-bit).
    unsafe impl Encode for Point {
        const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
    }
    unsafe impl Encode for Size {
        const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
    }
    unsafe impl Encode for Rect {
        const ENCODING: Encoding = Encoding::Struct("CGRect", &[Point::ENCODING, Size::ENCODING]);
    }

    /// `NSVisualEffectMaterialUnderWindowBackground`, `NSVisualEffectBlendingModeBehindWindow`,
    /// `NSVisualEffectStateActive`, `NSViewWidthSizable | NSViewHeightSizable`, `NSWindowBelow`.
    const UNDER_WINDOW_BACKGROUND: isize = 21;
    const BEHIND_WINDOW: isize = 0;
    const ACTIVE: isize = 1;
    const SIZABLE: usize = 2 | 16;
    const BELOW: isize = -1;

    thread_local! {
        /// Diskman's blur view (retained), while it is in the window.
        static VIEW: Cell<*mut AnyObject> = const { Cell::new(std::ptr::null_mut()) };
    }

    pub fn os_major() -> isize {
        // SAFETY: `NSProcessInfo.operatingSystemVersion` (macOS 10.10+), any thread.
        unsafe {
            let info: *mut AnyObject = msg_send![class!(NSProcessInfo), processInfo];
            if info.is_null() {
                return 0;
            }
            let v: Version = msg_send![info, operatingSystemVersion];
            let _ = (v.minor, v.patch);
            v.major
        }
    }

    pub fn installed() -> bool {
        VIEW.with(|v| !v.get().is_null())
    }

    pub fn layout() -> Option<(bool, (f64, f64), (f64, f64))> {
        let view = VIEW.with(|v| v.get());
        if view.is_null() {
            return None;
        }
        // SAFETY: main thread; `view` is retained by this module while installed.
        unsafe {
            let content: *mut AnyObject = msg_send![view, superview];
            if content.is_null() {
                return None;
            }
            let subviews: *mut AnyObject = msg_send![content, subviews];
            let first: *mut AnyObject = msg_send![subviews, firstObject];
            let own: Rect = msg_send![view, frame];
            let all: Rect = msg_send![content, bounds];
            Some((first == view, (own.size.w, own.size.h), (all.size.w, all.size.h)))
        }
    }

    /// GPUI's view (an `NSView` in the window's content view).
    fn gpui_view(window: &Window) -> Option<*mut AnyObject> {
        match HasWindowHandle::window_handle(window).ok()?.as_raw() {
            RawWindowHandle::AppKit(h) => Some(h.ns_view.as_ptr().cast()),
            _ => None,
        }
    }

    pub fn set_own_view(window: &Window, on: bool) {
        if on == installed() {
            return;
        }
        // SAFETY: AppKit calls on the main thread (GPUI runs the app there), on views of this
        // window: GPUI's view, its superview (the window's content view) and a stock
        // NSVisualEffectView this module allocates, keeps one reference to and releases once
        // it is out of the hierarchy.
        unsafe {
            if !on {
                let view = VIEW.with(|v| v.replace(std::ptr::null_mut()));
                let _: () = msg_send![view, removeFromSuperview];
                let _: () = msg_send![view, release];
                return;
            }
            let Some(gpui) = gpui_view(window) else { return };
            let content: *mut AnyObject = msg_send![gpui, superview];
            if content.is_null() {
                return;
            }
            let frame: Rect = msg_send![content, bounds];
            let view: *mut AnyObject = msg_send![class!(NSVisualEffectView), alloc];
            let view: *mut AnyObject = msg_send![view, initWithFrame: frame];
            if view.is_null() {
                return;
            }
            let _: () = msg_send![view, setAutoresizingMask: SIZABLE];
            let _: () = msg_send![view, setMaterial: UNDER_WINDOW_BACKGROUND];
            let _: () = msg_send![view, setBlendingMode: BEHIND_WINDOW];
            let _: () = msg_send![view, setState: ACTIVE];
            let nil: *mut AnyObject = std::ptr::null_mut();
            let _: () = msg_send![content, addSubview: view, positioned: BELOW, relativeTo: nil];
            VIEW.with(|v| v.set(view));
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn installed() -> bool {
        false
    }

    pub fn layout() -> Option<(bool, (f64, f64), (f64, f64))> {
        None
    }

    pub fn set_own_view(_: &gpui::Window, _: bool) {}
}
