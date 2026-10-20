//! GPUI's `prompt_for_paths` has no file type filter at this revision, so on macOS the panels
//! are AppKit's own `NSOpenPanel` / `NSSavePanel`.

use std::path::PathBuf;
use std::sync::Mutex;

use gpui::App;
use tokio::sync::oneshot;

static TEST_PATHS: Mutex<Option<Vec<PathBuf>>> = Mutex::new(None);
static TEST_SAVE: Mutex<Option<PathBuf>> = Mutex::new(None);

#[cfg(feature = "checks")]
pub fn set_test_paths(paths: Vec<PathBuf>) {
    *TEST_PATHS.lock().unwrap() = Some(paths);
}

#[cfg(feature = "checks")]
pub fn set_test_save_path(path: PathBuf) {
    *TEST_SAVE.lock().unwrap() = Some(path);
}

pub fn file_name_for(title: &str) -> String {
    let clean: String = title.chars().map(|c| if matches!(c, '/' | ':' | '\\') || c.is_control() { ' ' } else { c }).collect();
    let stem = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    let stem = stem.trim_end_matches('.');
    let stem = if stem.is_empty() { "query" } else { stem };
    if is_sql(std::path::Path::new(stem)) { stem.to_string() } else { format!("{stem}.sql") }
}

pub fn pick_save_path(name: &str, cx: &App) -> oneshot::Receiver<Option<PathBuf>> {
    let (tx, rx) = oneshot::channel();
    if let Some(path) = TEST_SAVE.lock().unwrap().take() {
        let _ = tx.send(Some(path));
        return rx;
    }
    #[cfg(target_os = "macos")]
    {
        let _ = cx;
        imp::save_panel(name, tx);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let dir = std::env::current_dir().unwrap_or_default();
        let path = cx.prompt_for_new_path(&dir, Some(name));
        cx.spawn(async move |_| {
            let picked = match path.await {
                Ok(Ok(Some(p))) => Some(if is_sql(&p) { p } else { p.with_extension("sql") }),
                _ => None,
            };
            let _ = tx.send(picked);
        })
        .detach();
    }
    rx
}

pub fn is_sql(p: &std::path::Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("sql"))
}

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

#[cfg(feature = "checks")]
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

    #[cfg(feature = "checks")]
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

    /// SAFETY: `panel` is an `NSSavePanel` (or `NSOpenPanel`), on the main thread.
    unsafe fn allow_sql(panel: &AnyObject) {
        let ext = ns_string("sql");
        unsafe {
            match AnyClass::get(c"UTType") {
                Some(ut) => {
                    let ty: Option<Retained<AnyObject>> = msg_send![ut, typeWithFilenameExtension: &*ext];
                    if let Some(ty) = ty {
                        let types: Retained<AnyObject> = msg_send![class!(NSArray), arrayWithObject: &*ty];
                        let _: () = msg_send![panel, setAllowedContentTypes: &*types];
                    }
                }
                None => {
                    let types: Retained<AnyObject> = msg_send![class!(NSArray), arrayWithObject: &*ext];
                    let _: () = msg_send![panel, setAllowedFileTypes: &*types];
                }
            }
        }
    }

    /// SAFETY: `url` is an `NSURL`.
    unsafe fn url_path(url: &AnyObject) -> Option<PathBuf> {
        unsafe {
            let path: Option<Retained<AnyObject>> = msg_send![url, path];
            let path = path?;
            let c: *const std::ffi::c_char = msg_send![&*path, UTF8String];
            (!c.is_null()).then(|| PathBuf::from(CStr::from_ptr(c).to_string_lossy().into_owned()))
        }
    }

    pub fn save_panel(name: &str, tx: oneshot::Sender<Option<PathBuf>>) {
        // SAFETY: AppKit on the main thread (GPUI's UI thread calls this), with retained
        // objects only; the block is kept alive by the panel until it has run.
        unsafe {
            let panel: Retained<AnyObject> = msg_send![class!(NSSavePanel), savePanel];
            let _: () = msg_send![&*panel, setCanCreateDirectories: Bool::YES];
            allow_sql(&panel);
            let n = ns_string(name);
            let _: () = msg_send![&*panel, setNameFieldStringValue: &*n];
            let tx = Cell::new(Some(tx));
            let p = panel.clone();
            let handler = RcBlock::new(move |response: isize| {
                let Some(tx) = tx.take() else { return };
                let mut out = None;
                if response == OK {
                    let url: Option<Retained<AnyObject>> = msg_send![&*p, URL];
                    out = url.and_then(|u| url_path(&u));
                }
                let _ = tx.send(out);
            });
            let _: () = msg_send![&*panel, beginWithCompletionHandler: &*handler];
        }
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
            allow_sql(&panel);
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
                        out.extend(url_path(&url));
                    }
                }
                let _ = tx.send(out.into_iter().filter(|p| super::is_sql(p)).collect());
            });
            let _: () = msg_send![&*panel, beginWithCompletionHandler: &*handler];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_from_titles() {
        assert_eq!(file_name_for("Orders by day"), "Orders by day.sql");
        assert_eq!(file_name_for("report.sql"), "report.sql");
        assert_eq!(file_name_for("a/b: c"), "a b c.sql");
        assert_eq!(file_name_for("SELECT * FROM t…"), "SELECT * FROM t….sql");
        assert_eq!(file_name_for("  "), "query.sql");
    }
}
