use std::sync::Mutex;

static SHOWN: Mutex<Option<bool>> = Mutex::new(None);

pub fn follow(light: bool) {
    let Ok(mut shown) = SHOWN.lock() else { return };
    if *shown == Some(light) {
        return;
    }
    if shown.is_none() && !light && imp::bundled() {
        *shown = Some(false);
        return;
    }
    if imp::set(light) {
        *shown = Some(light);
    }
}

pub fn shown_light() -> Option<bool> {
    SHOWN.lock().ok().and_then(|s| *s)
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::CString;
    use std::path::PathBuf;

    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    fn packaging() -> PathBuf {
        let exe = std::env::current_exe().unwrap_or_default();
        exe.ancestors().nth(3).map(|root| root.join("packaging")).unwrap_or_default()
    }

    fn ns_string(s: &str) -> *mut AnyObject {
        let c = CString::new(s).unwrap_or_default();
        // SAFETY: a NUL-terminated UTF-8 string; `stringWithUTF8String:` copies it.
        unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] }
    }

    fn resource(name: &str, ext: &str) -> Option<String> {
        // SAFETY: NSBundle's main bundle, any thread; a nil path is "not found".
        unsafe {
            let bundle: *mut AnyObject = msg_send![class!(NSBundle), mainBundle];
            if bundle.is_null() {
                return None;
            }
            let path: *mut AnyObject = msg_send![bundle, pathForResource: ns_string(name), ofType: ns_string(ext)];
            if path.is_null() {
                return None;
            }
            let c: *const std::ffi::c_char = msg_send![path, UTF8String];
            (!c.is_null()).then(|| std::ffi::CStr::from_ptr(c).to_string_lossy().into_owned())
        }
    }

    pub fn bundled() -> bool {
        resource("AppIcon", "icns").is_some()
    }

    fn file(light: bool) -> Option<String> {
        if light {
            resource("AppIconLight", "icns").or_else(|| Some(packaging().join("AppIconLight.icns").to_string_lossy().into_owned()))
        } else if bundled() {
            None
        } else {
            Some(packaging().join("AppIcon.icns").to_string_lossy().into_owned())
        }
    }

    pub fn set(light: bool) -> bool {
        let path = file(light);
        if path.as_deref().is_some_and(|p| !std::path::Path::new(p).exists()) {
            return false;
        }
        // SAFETY: AppKit on the main thread (the theme changes there); the image is released
        // once the application holds it; nil restores the bundle's icon.
        unsafe {
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            if app.is_null() {
                return false;
            }
            let image: *mut AnyObject = match &path {
                Some(p) => {
                    let alloc: *mut AnyObject = msg_send![class!(NSImage), alloc];
                    let image: *mut AnyObject = msg_send![alloc, initWithContentsOfFile: ns_string(p)];
                    if image.is_null() {
                        return false;
                    }
                    image
                }
                None => std::ptr::null_mut(),
            };
            let _: () = msg_send![app, setApplicationIconImage: image];
            if !image.is_null() {
                let _: () = msg_send![image, release];
            }
        }
        true
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn bundled() -> bool {
        true
    }

    pub fn set(_light: bool) -> bool {
        true
    }
}
