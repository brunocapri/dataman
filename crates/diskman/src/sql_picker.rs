//! The `.sql` file picker (Open .sql file, Import favorites): the system open panel showing
//! only `.sql` files, one or several at once. GPUI's `prompt_for_paths` has no file type
//! filter at this revision, so on macOS the panel is AppKit's own `NSOpenPanel`
//! (`allowedContentTypes` = the `sql` extension's type), begun as a sheet-less panel with a
//! completion handler: the UI keeps running while it is open. Checks hand it their paths
//! instead ([`set_test_paths`]).

use std::path::PathBuf;
use std::sync::Mutex;

use gpui::App;
use tokio::sync::oneshot;

static TEST_PATHS: Mutex<Option<Vec<PathBuf>>> = Mutex::new(None);

/// Checks: the next pick answers with these paths, without a panel.
#[allow(dead_code)]
pub fn set_test_paths(paths: Vec<PathBuf>) {
    *TEST_PATHS.lock().unwrap() = Some(paths);
}

/// Whether `p` is a `.sql` file (any case).
pub fn is_sql(p: &std::path::Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("sql"))
}

/// Asks for `.sql` files (`multiple`: one or more): the chosen paths, empty when cancelled.
pub fn pick_sql_files(multiple: bool, cx: &App) -> oneshot::Receiver<Vec<PathBuf>> {
    let (tx, rx) = oneshot::channel();
    if let Some(paths) = TEST_PATHS.lock().unwrap().take() {
        let _ = tx.send(paths);
        return rx;
    }
    #[cfg(target_os = "macos")]
    {
        let _ = cx;
        imp::open_panel(multiple, tx);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions { files: true, directories: false, multiple, prompt: None });
        cx.spawn(async move |_| {
            let picked = match paths.await {
                Ok(Ok(Some(p))) => p.into_iter().filter(|p| is_sql(p)).collect(),
                _ => Vec::new(),
            };
            let _ = tx.send(picked);
        })
        .detach();
    }
    rx
}

/// Demo stage `sql-panel`: opens the real panel, cancels it after a moment and prints whether
/// the completion came back (empty), so the AppKit path is exercised without a user.
pub async fn demo_panel(cx: &mut gpui::AsyncWindowContext) -> bool {
    let Ok(rx) = cx.update(|_, cx| pick_sql_files(true, cx)) else { return false };
    cx.background_executor().timer(std::time::Duration::from_millis(1200)).await;
    #[cfg(target_os = "macos")]
    let cancelled = cx.update(|_, _| imp::cancel_panels()).unwrap_or(0);
    #[cfg(not(target_os = "macos"))]
    let cancelled = 1;
    let got = rx.await;
    let ok = cancelled == 1 && got.as_ref().is_ok_and(|p| p.is_empty());
    println!("{} the .sql open panel opens and its Cancel answers with no files — {cancelled} panel(s), {got:?}", if ok { "PASS" } else { "FAIL" });
    ok
}

#[cfg(target_os = "macos")]
mod imp {
    use std::cell::Cell;
    use std::ffi::{CStr, CString};
    use std::path::PathBuf;

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2::{class, msg_send};
    use tokio::sync::oneshot;

    /// `NSModalResponseOK`.
    const OK: isize = 1;

    fn ns_string(s: &str) -> Retained<AnyObject> {
        let c = CString::new(s).unwrap_or_default();
        // SAFETY: a NUL-terminated UTF-8 string; `stringWithUTF8String:` copies it.
        unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] }
    }

    /// Demo (`sql-panel`): cancels every open panel of the app, as its Cancel button does.
    pub fn cancel_panels() -> usize {
        let mut n = 0;
        // SAFETY: AppKit on the main thread; `windows` and `cancel:` as documented.
        unsafe {
            let app: Retained<AnyObject> = msg_send![class!(NSApplication), sharedApplication];
            let windows: Retained<AnyObject> = msg_send![&*app, windows];
            let count: usize = msg_send![&*windows, count];
            let panel = class!(NSOpenPanel);
            for i in 0..count {
                let w: Retained<AnyObject> = msg_send![&*windows, objectAtIndex: i];
                let is: Bool = msg_send![&*w, isKindOfClass: panel];
                let visible: Bool = msg_send![&*w, isVisible];
                if is.as_bool() && visible.as_bool() {
                    let _: () = msg_send![&*w, cancel: std::ptr::null::<AnyObject>()];
                    n += 1;
                }
            }
        }
        n
    }

    pub fn open_panel(multiple: bool, tx: oneshot::Sender<Vec<PathBuf>>) {
        // SAFETY: AppKit on the main thread (GPUI's UI thread calls this), with retained
        // objects only; the block is kept alive by the panel until it has run.
        unsafe {
            let panel: Retained<AnyObject> = msg_send![class!(NSOpenPanel), openPanel];
            let _: () = msg_send![&*panel, setCanChooseFiles: Bool::YES];
            let _: () = msg_send![&*panel, setCanChooseDirectories: Bool::NO];
            let _: () = msg_send![&*panel, setAllowsMultipleSelection: Bool::new(multiple)];
            let _: () = msg_send![&*panel, setResolvesAliases: Bool::YES];
            let ext = ns_string("sql");
            // macOS 11+: the extension's content type; older systems: the extension itself.
            match AnyClass::get(c"UTType") {
                Some(ut) => {
                    let ty: Option<Retained<AnyObject>> = msg_send![ut, typeWithFilenameExtension: &*ext];
                    if let Some(ty) = ty {
                        let types: Retained<AnyObject> = msg_send![class!(NSArray), arrayWithObject: &*ty];
                        let _: () = msg_send![&*panel, setAllowedContentTypes: &*types];
                    }
                }
                None => {
                    let types: Retained<AnyObject> = msg_send![class!(NSArray), arrayWithObject: &*ext];
                    let _: () = msg_send![&*panel, setAllowedFileTypes: &*types];
                }
            }
            let tx = Cell::new(Some(tx));
            let p = panel.clone();
            let handler = RcBlock::new(move |response: isize| {
                let Some(tx) = tx.take() else { return };
                let mut out = Vec::new();
                if response == OK {
                    let urls: Retained<AnyObject> = msg_send![&*p, URLs];
                    let n: usize = msg_send![&*urls, count];
                    for i in 0..n {
                        let url: Retained<AnyObject> = msg_send![&*urls, objectAtIndex: i];
                        let path: Option<Retained<AnyObject>> = msg_send![&*url, path];
                        let Some(path) = path else { continue };
                        let c: *const std::ffi::c_char = msg_send![&*path, UTF8String];
                        if !c.is_null() {
                            out.push(PathBuf::from(CStr::from_ptr(c).to_string_lossy().into_owned()));
                        }
                    }
                }
                let _ = tx.send(out.into_iter().filter(|p| super::is_sql(p)).collect());
            });
            let _: () = msg_send![&*panel, beginWithCompletionHandler: &*handler];
        }
    }
}
