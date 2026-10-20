// GPUI's blur (`set_background_appearance`) edits the NSVisualEffectView's private layer tree
// by class names and filter descriptions; on macOS 26 the blur is lost. So there (it reports
// itself as 16.x to binaries built with an older SDK) Dataman asks GPUI for a transparent
// window and puts its own stock NSVisualEffectView, layers left alone, under GPUI's view.

use gpui::{Window, WindowBackgroundAppearance};
use dataman_core::Translucency;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blur {
    Gpui,
    Own,
}

pub fn blur() -> Blur {
    static B: std::sync::OnceLock<Blur> = std::sync::OnceLock::new();
    *B.get_or_init(|| {
        let b = match std::env::var("DATAMAN_BLUR").ok().as_deref() {
            Some("own") => Blur::Own,
            Some("gpui") => Blur::Gpui,
            _ if macos_major() >= 16 => Blur::Own,
            _ => Blur::Gpui,
        };
        log::info!("window blur: {b:?} (macOS {})", macos_major());
        b
    })
}

pub fn background_for(level: Translucency) -> WindowBackgroundAppearance {
    match (level.is_on(), blur()) {
        (false, _) => WindowBackgroundAppearance::Opaque,
        (true, Blur::Gpui) => WindowBackgroundAppearance::Blurred,
        (true, Blur::Own) => WindowBackgroundAppearance::Transparent,
    }
}

pub fn apply(level: Translucency, window: &mut Window) {
    window.set_background_appearance(background_for(level));
    imp::set_own_view(window, level.is_on() && blur() == Blur::Own);
}

#[cfg(feature = "checks")]
pub fn own_view_installed() -> bool {
    imp::installed()
}

#[cfg(feature = "checks")]
pub fn own_view_layout() -> Option<(bool, (f64, f64), (f64, f64))> {
    imp::layout()
}

#[cfg(target_os = "macos")]
fn macos_major() -> isize {
    imp::os_major()
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn major_of(text: &str) -> isize {
    text.split(|c: char| !c.is_ascii_digit()).find(|s| !s.is_empty()).and_then(|s| s.parse().ok()).unwrap_or(0)
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
        static VIEW: Cell<*mut AnyObject> = const { Cell::new(std::ptr::null_mut()) };
    }

    // From the string: the struct of `operatingSystemVersion` is anonymous on some systems
    // (`{?=qqq}`), which objc2's encoding check rejects in debug builds.
    pub fn os_major() -> isize {
        // SAFETY: `NSProcessInfo.operatingSystemVersionString` (macOS 10.2+), any thread; the
        // UTF-8 buffer lives as long as the string, read before returning.
        let text = unsafe {
            let info: *mut AnyObject = msg_send![class!(NSProcessInfo), processInfo];
            if info.is_null() {
                return 0;
            }
            let s: *mut AnyObject = msg_send![info, operatingSystemVersionString];
            if s.is_null() {
                return 0;
            }
            let c: *const std::ffi::c_char = msg_send![s, UTF8String];
            if c.is_null() {
                return 0;
            }
            std::ffi::CStr::from_ptr(c).to_string_lossy().into_owned()
        };
        super::major_of(&text)
    }

    pub fn installed() -> bool {
        VIEW.with(|v| !v.get().is_null())
    }

    #[cfg(feature = "checks")]
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
    #[cfg(feature = "checks")]
    pub fn installed() -> bool {
        false
    }

    #[cfg(feature = "checks")]
    pub fn layout() -> Option<(bool, (f64, f64), (f64, f64))> {
        None
    }

    pub fn set_own_view(_: &gpui::Window, _: bool) {}
}

#[cfg(test)]
mod tests {
    #[test]
    fn major_of() {
        assert_eq!(super::major_of("Version 15.6 (Build 24G84)"), 15);
        assert_eq!(super::major_of("Version 26.0 (Build 25A354)"), 26);
        assert_eq!(super::major_of("unknown"), 0);
    }
}
