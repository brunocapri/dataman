//! The panic hook writes the report first thing: GPUI's platform callbacks are `extern "C"`,
//! so a panic inside one aborts the process right after the hook returns.

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::SystemTime;

use anyhow::{Context as _, Result, bail};
use parking_lot::Mutex;

pub const LOG_LINES: usize = 200;
const KEEP: usize = 20;
const STAMP: &str = ".last-launch";

static LINES: Mutex<VecDeque<String>> = parking_lot::const_mutex(VecDeque::new());
static HOOK: OnceLock<(PathBuf, String)> = OnceLock::new();
static WRITTEN: Mutex<Option<PathBuf>> = parking_lot::const_mutex(None);

pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("DATAMAN_LOG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    if crate::os::LINUX {
        return crate::os::state_home().unwrap_or_else(std::env::temp_dir).join("dataman/logs");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    home.join("Library/Logs/Dataman")
}

/// Linux keeps no per-app crash reports: an empty folder there.
pub fn system_dir() -> PathBuf {
    if crate::os::LINUX {
        return dir().join("system");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    home.join("Library/Logs/DiagnosticReports")
}

struct Logger {
    stderr: bool,
}

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Warn || (m.level() <= log::Level::Info && m.target().starts_with("dataman"))
    }

    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let line = format!("{} {:<5} {}: {}", chrono::Utc::now().format("%H:%M:%S%.3f"), r.level(), r.target(), r.args());
        if self.stderr {
            eprintln!("{line}");
        }
        let mut g = LINES.lock();
        if g.len() == LOG_LINES {
            g.pop_front();
        }
        g.push_back(line);
    }

    fn flush(&self) {}
}

pub fn install(dir: PathBuf, version: &str) {
    let _ = HOOK.set((dir, version.to_string()));
    let logger = Box::leak(Box::new(Logger { stderr: std::env::var_os("DATAMAN_LOG").is_some() }));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |p| {
        write_report(p);
        prev(p);
    }));
}

fn payload<'a>(p: &'a std::panic::PanicHookInfo<'a>) -> &'a str {
    if let Some(s) = p.payload().downcast_ref::<&str>() {
        s
    } else if let Some(s) = p.payload().downcast_ref::<String>() {
        s
    } else {
        "(no message)"
    }
}

fn write_report(p: &std::panic::PanicHookInfo) {
    let Some((dir, version)) = HOOK.get() else { return };
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("(unnamed)");
    let msg = payload(p);
    let loc = p.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())).unwrap_or_default();
    // The lock may be held by the code that panicked: never wait for it.
    let Some(mut written) = WRITTEN.try_lock() else { return };
    if let Some(path) = written.as_ref() {
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path) {
            let _ = writeln!(f, "\nthen, on thread {name}: {msg}\n  at {loc}");
        }
        return;
    }
    let _ = std::fs::create_dir_all(dir);
    let now = chrono::Utc::now();
    let path = dir.join(format!("crash-{}.txt", now.format("%Y%m%d-%H%M%S")));
    let Ok(mut f) = std::fs::File::create(&path) else { return };
    *written = Some(path);
    drop(written);
    let backtrace = std::backtrace::Backtrace::force_capture();
    let lines = LINES.try_lock().map(|g| g.iter().cloned().collect::<Vec<_>>());
    let _ = write_to(&mut f, version, &now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true), name, msg, &loc, &backtrace.to_string(), lines.as_deref());
    let _ = f.sync_all();
}

#[allow(clippy::too_many_arguments)]
pub fn write_to(
    w: &mut impl std::io::Write,
    version: &str,
    when: &str,
    thread: &str,
    msg: &str,
    loc: &str,
    backtrace: &str,
    log: Option<&[String]>,
) -> std::io::Result<()> {
    let os = crate::os::version().unwrap_or_else(|| "?".into());
    writeln!(w, "Dataman {version} crash report")?;
    writeln!(w, "time      {when}")?;
    writeln!(w, "{:<10}{os} ({})", crate::os::NAME, std::env::consts::ARCH)?;
    writeln!(w, "process   {}", std::process::id())?;
    writeln!(w, "thread    {thread}")?;
    writeln!(w, "panic     {msg}")?;
    writeln!(w, "location  {loc}")?;
    writeln!(w, "\nbacktrace\n{backtrace}")?;
    match log {
        Some(lines) => {
            writeln!(w, "\nlog (last {})", lines.len())?;
            for l in lines {
                writeln!(w, "{l}")?;
            }
        }
        None => writeln!(w, "\nlog (busy)")?,
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub report: Option<PathBuf>,
    pub system: Option<PathBuf>,
    pub at: SystemTime,
}

impl Pending {
    pub fn primary(&self) -> &Path {
        self.report.as_deref().or(self.system.as_deref()).expect("a pending crash has a file")
    }

    pub fn text(&self) -> String {
        let mut s = match &self.report {
            Some(p) => std::fs::read_to_string(p).unwrap_or_else(|e| format!("{}: {e}", p.display())),
            None => String::new(),
        };
        if let Some(sys) = &self.system {
            if !s.is_empty() {
                s.push('\n');
            }
            match &self.report {
                Some(_) => s.push_str(&format!("{} report: {}\n", crate::os::NAME, sys.display())),
                None => s.push_str(&std::fs::read_to_string(sys).unwrap_or_else(|e| format!("{}: {e}", sys.display()))),
            }
        }
        s
    }
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn newest(dir: &Path, keep: impl Fn(&str) -> bool, after: Option<SystemTime>) -> Option<(PathBuf, SystemTime)> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(&keep))
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
        .filter(|(_, t)| after.is_none_or(|a| *t > a))
        .max_by_key(|(_, t)| *t)
}

pub fn take_pending(dir: &Path, system: &Path) -> Option<Pending> {
    let stamp = dir.join(STAMP);
    let last = mtime(&stamp);
    let report = newest(dir, |n| n.starts_with("crash-") && n.ends_with(".txt"), last);
    let sys = last.and_then(|l| newest(system, |n| n.starts_with("dataman") && n.ends_with(".ips"), Some(l)));
    if std::fs::create_dir_all(dir).is_ok() {
        let _ = std::fs::write(&stamp, b"");
    }
    prune(dir);
    let at = report.iter().chain(sys.iter()).map(|(_, t)| *t).max()?;
    Some(Pending { report: report.map(|(p, _)| p), system: sys.map(|(p, _)| p), at })
}

fn prune(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut all: Vec<(PathBuf, SystemTime)> = rd
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with("crash-") && n.ends_with(".txt")))
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
        .collect();
    if all.len() <= KEEP {
        return;
    }
    all.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
    for (p, _) in all.into_iter().skip(KEEP) {
        let _ = std::fs::remove_file(p);
    }
}

pub trait Reporter: Send + Sync {
    fn send(&self, text: &str) -> Result<()>;
}

pub struct Endpoint(pub String);

impl Reporter for Endpoint {
    fn send(&self, text: &str) -> Result<()> {
        let mut child = Command::new("/usr/bin/curl")
            .args(["--fail", "--silent", "--show-error", "--max-time", "30", "--proto", "=https,http", "-X", "POST"])
            .args(["-H", "Content-Type: text/plain; charset=utf-8", "--data-binary", "@-"])
            .arg(&self.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("curl")?;
        child.stdin.take().context("curl stdin")?.write_all(text.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }
}

pub fn reporter(endpoint: Option<&str>) -> Option<Box<dyn Reporter>> {
    endpoint.map(str::trim).filter(|u| !u.is_empty()).map(|u| Box::new(Endpoint(u.to_string())) as Box<dyn Reporter>)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn touch(p: &Path, t: SystemTime) {
        std::fs::write(p, b"x").unwrap();
        std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn report_text() {
        let mut out = Vec::new();
        let log = vec!["12:00:00.000 WARN  dataman: one".to_string()];
        write_to(&mut out, "0.1.0", "2026-10-13T12:00:00Z", "main", "boom", "src/x.rs:1:2", "0: f\n1: g", Some(&log)).unwrap();
        let s = String::from_utf8(out).unwrap();
        let os = format!("{:<10}", crate::os::NAME);
        for want in ["Dataman 0.1.0", "thread    main", "panic     boom", "location  src/x.rs:1:2", "0: f", "log (last 1)", "dataman: one", os.as_str()] {
            assert!(s.contains(want), "{want} in {s}");
        }
    }

    #[test]
    fn pending_after_last_launch() {
        let tmp = tempfile::tempdir().unwrap();
        let (dir, sys) = (tmp.path().join("logs"), tmp.path().join("diag"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&sys).unwrap();
        let now = SystemTime::now();
        let ago = |s: u64| now - Duration::from_secs(s);
        touch(&sys.join("dataman-2026-10-01-000000.ips"), ago(5000));
        touch(&dir.join("crash-20261001-000000.txt"), ago(4000));
        let p = take_pending(&dir, &sys).unwrap();
        assert_eq!((p.report.as_deref(), p.system.as_deref()), (Some(dir.join("crash-20261001-000000.txt").as_path()), None));
        assert_eq!(take_pending(&dir, &sys), None);
        std::fs::File::options().write(true).open(dir.join(STAMP)).unwrap().set_modified(ago(100)).unwrap();
        touch(&dir.join("crash-20261013-000000.txt"), ago(50));
        touch(&sys.join("dataman-2026-10-13-000000.ips"), ago(40));
        touch(&sys.join("Other-2026-10-13-000000.ips"), ago(30));
        let p = take_pending(&dir, &sys).unwrap();
        assert_eq!(p.primary(), dir.join("crash-20261013-000000.txt"));
        assert_eq!(p.system.as_deref(), Some(sys.join("dataman-2026-10-13-000000.ips").as_path()));
        assert!(p.text().contains(&format!("{} report: ", crate::os::NAME)));
        std::fs::File::options().write(true).open(dir.join(STAMP)).unwrap().set_modified(ago(20)).unwrap();
        touch(&sys.join("dataman-2026-10-13-000100.ips"), ago(10));
        let p = take_pending(&dir, &sys).unwrap();
        assert_eq!((p.report.clone(), p.primary().to_path_buf()), (None, sys.join("dataman-2026-10-13-000100.ips")));
        assert_eq!(p.text(), "x");
    }

    #[test]
    fn prunes_old_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        for i in 0..25u64 {
            touch(&tmp.path().join(format!("crash-{i:02}.txt")), now - Duration::from_secs(1000 - i));
        }
        prune(tmp.path());
        let left = std::fs::read_dir(tmp.path()).unwrap().count();
        assert_eq!(left, KEEP);
        assert!(tmp.path().join("crash-24.txt").exists() && !tmp.path().join("crash-00.txt").exists());
    }

    #[test]
    fn reporter_only_with_endpoint() {
        assert!(reporter(None).is_none());
        assert!(reporter(Some("  ")).is_none());
        assert!(reporter(Some("https://example.invalid/crash")).is_some());
    }
}
