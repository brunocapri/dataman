//! A Notification Center banner when the update check finds a new version while Dataman is in
//! the background (UserNotifications: a bundle with an identifier is enough, no entitlement;
//! macOS asks once whether Dataman may notify). Nothing when the app is in front (the title
//! bar's notice says it), when it does not run from a bundle, or under `DATAMAN_NO_NOTIFY`.

use gpui::App;

pub fn update_available(version: &str, _cx: &App) {
    if std::env::var_os("DATAMAN_NO_NOTIFY").is_some() {
        return;
    }
    #[cfg(target_os = "macos")]
    imp::post(&format!("Dataman {version} is available"), "Open Dataman to install it.");
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::CString;

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2::{class, msg_send};

    #[link(name = "UserNotifications", kind = "framework")]
    unsafe extern "C" {}

    /// `UNAuthorizationOptionSound | UNAuthorizationOptionAlert`.
    const OPTIONS: usize = (1 << 1) | (1 << 2);

    fn ns_string(s: &str) -> Retained<AnyObject> {
        let c = CString::new(s).unwrap_or_default();
        // SAFETY: a NUL-terminated UTF-8 string; `stringWithUTF8String:` copies it.
        unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] }
    }

    /// Whether the app is in front, and runs from a bundle with an identifier (the notification
    /// center raises an exception for a process without one).
    fn can_post() -> bool {
        let bundled = std::env::current_exe().ok().is_some_and(|e| dataman_core::update::bundle_of(&e).is_some());
        // SAFETY: AppKit's shared application and the main bundle, read on the main thread.
        unsafe {
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            let active: Bool = msg_send![app, isActive];
            let bundle: *mut AnyObject = msg_send![class!(NSBundle), mainBundle];
            let id: *mut AnyObject = if bundle.is_null() { std::ptr::null_mut() } else { msg_send![bundle, bundleIdentifier] };
            bundled && !active.as_bool() && !id.is_null()
        }
    }

    pub fn post(title: &str, body: &str) {
        if !can_post() {
            return;
        }
        let (Some(center_cls), Some(content_cls), Some(request_cls)) =
            (AnyClass::get(c"UNUserNotificationCenter"), AnyClass::get(c"UNMutableNotificationContent"), AnyClass::get(c"UNNotificationRequest"))
        else {
            return;
        };
        // SAFETY: UserNotifications as documented; the handlers run on its queue and only add the
        // request, which the center allows from any thread.
        unsafe {
            let center: Retained<AnyObject> = msg_send![center_cls, currentNotificationCenter];
            let content: Retained<AnyObject> = msg_send![content_cls, new];
            let _: () = msg_send![&*content, setTitle: &*ns_string(title)];
            let _: () = msg_send![&*content, setBody: &*ns_string(body)];
            let id = ns_string("dataman-update");
            let request: Retained<AnyObject> = msg_send![request_cls, requestWithIdentifier: &*id, content: &*content, trigger: std::ptr::null::<AnyObject>()];
            let c = center.clone();
            let handler = RcBlock::new(move |granted: Bool, _err: *mut AnyObject| {
                if !granted.as_bool() {
                    log::info!("update: notifications not allowed");
                    return;
                }
                let done = RcBlock::new(|err: *mut AnyObject| {
                    if !err.is_null() {
                        log::warn!("update: the notification was not posted");
                    }
                });
                let _: () = msg_send![&*c, addNotificationRequest: &*request, withCompletionHandler: &*done];
            });
            let _: () = msg_send![&*center, requestAuthorizationWithOptions: OPTIONS, completionHandler: &*handler];
        }
    }
}
