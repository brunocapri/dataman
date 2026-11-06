//! One Dataman per user on Linux and Windows, as macOS keeps one per app: a second launch hands
//! its files to the first over an owner-only socket and exits. GPUI does not deliver files opened
//! with the app there, so they come from the command line. On Windows the socket is a named pipe
//! whose random name is in `instance.pipe` in the config folder (the user's own).

use std::io::Write as _;
#[cfg(unix)]
use std::io::{BufRead as _, BufReader};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use dataman_core::Dirs;

/// `file://` URLs for the paths on the command line (what `on_open_urls` would bring).
pub fn urls_from_args(args: &[String]) -> Vec<String> {
    args.iter()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .filter_map(|a| dataman_core::os::canonical(a).ok())
        .filter(|p| p.is_file())
        .map(|p| file_url(&p))
        .collect()
}

pub fn file_url(p: &Path) -> String {
    let mut out = String::from("file://");
    let s = p.to_string_lossy();
    // `\\?\C:\a\b.sql` (what canonicalize gives) → `/C:/a/b.sql`.
    #[cfg(windows)]
    let s = match s.strip_prefix(r"\\?\").unwrap_or(&s).replace('\\', "/") {
        s if s.starts_with('/') => s,
        s => format!("/{s}"),
    };
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(windows)]
pub fn socket_path(dirs: &Dirs) -> PathBuf {
    dirs.config.join("instance.pipe")
}

#[cfg(unix)]
pub fn socket_path(dirs: &Dirs) -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_absolute() && d.is_dir());
    match runtime {
        Some(d) => {
            let mut h: u64 = 0xcbf29ce484222325;
            for b in dirs.config.to_string_lossy().bytes() {
                h = (h ^ b as u64).wrapping_mul(0x100000001b3);
            }
            d.join(format!("dataman-{h:016x}.sock"))
        }
        None => dirs.config.join("instance.sock"),
    }
}

/// Another Dataman runs for these settings: it got the URLs (none: it comes to the front).
pub fn hand_over(socket: &Path, urls: &[String]) -> bool {
    #[cfg(unix)]
    let Ok(mut s) = UnixStream::connect(socket) else { return false };
    #[cfg(windows)]
    let Some(mut s) = std::fs::read_to_string(socket)
        .ok()
        .filter(|n| n.trim().starts_with(r"\\.\pipe\dataman-instance-"))
        .and_then(|n| std::fs::OpenOptions::new().write(true).open(n.trim()).ok())
    else {
        return false;
    };
    let mut msg = urls.join("\n");
    msg.push_str("\n\n");
    s.write_all(msg.as_bytes()).is_ok()
}

fn urls_in(lines: impl Iterator<Item = String>) -> Vec<String> {
    let mut urls = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if line.starts_with("file://") && urls.len() < 64 {
            urls.push(line);
        }
    }
    urls
}

/// Listens for later launches; each sends its URLs (an empty list: show the window).
#[cfg(windows)]
pub fn listen(path: PathBuf, on: impl Fn(Vec<String>) + Send + 'static) {
    use tokio::io::AsyncBufReadExt as _;
    use tokio::net::windows::named_pipe::ServerOptions;
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
    let name = format!(r"\\.\pipe\dataman-instance-{}", dataman_mcp::ipc::new_key());
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
    let create = {
        let name = name.clone();
        move |first: bool| ServerOptions::new().first_pipe_instance(first).reject_remote_clients(true).create(&name)
    };
    let mut server = match rt.block_on(async { create(true) }) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("instance pipe: {e}");
            return;
        }
    };
    if let Err(e) = std::fs::write(&path, &name) {
        log::warn!("instance pipe {}: {e}", path.display());
        return;
    }
    let _ = std::thread::Builder::new().name("instance".into()).spawn(move || {
        rt.block_on(async move {
            loop {
                let connected = server.connect().await;
                let Ok(next) = create(false) else { break };
                let s = std::mem::replace(&mut server, next);
                if connected.is_err() {
                    continue;
                }
                let mut lines = tokio::io::BufReader::new(s).lines();
                let mut got = Vec::new();
                while let Ok(Some(l)) = lines.next_line().await {
                    let end = l.is_empty();
                    got.push(l);
                    if end {
                        break;
                    }
                }
                on(urls_in(got.into_iter()));
            }
        })
    });
}

/// Listens for later launches; each sends its URLs (an empty list: show the window).
#[cfg(unix)]
pub fn listen(path: PathBuf, on: impl Fn(Vec<String>) + Send + 'static) {
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new("/")));
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            log::warn!("instance socket {}: {e}", path.display());
            return;
        }
    };
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    let _ = std::thread::Builder::new().name("instance".into()).spawn(move || {
        for s in listener.incoming().flatten() {
            on(urls_in(BufReader::new(s).lines().map_while(Result::ok)));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_for_paths() {
        assert_eq!(file_url(Path::new("/home/u/My Queries/a#1.sql")), "file:///home/u/My%20Queries/a%231.sql");
        assert_eq!(crate::app::file_url_path(&file_url(Path::new("/tmp/é x.sql"))), Some(PathBuf::from("/tmp/é x.sql")));
    }

    #[test]
    #[cfg(windows)]
    fn urls_for_windows_paths() {
        let url = file_url(Path::new(r"\\?\C:\Users\u\My Queries\a.sql"));
        assert_eq!(url, "file:///C%3A/Users/u/My%20Queries/a.sql");
        assert_eq!(crate::app::file_url_path(&url), Some(PathBuf::from(r"C:\Users\u\My Queries\a.sql")));
        assert_eq!(crate::app::file_url_path(r"D:\x.sql"), Some(PathBuf::from(r"D:\x.sql")));
    }

    #[test]
    fn a_second_launch_hands_its_files_over() {
        let dir = std::env::temp_dir().join(format!("dataman-instance-test-{}", std::process::id()));
        let sock = dir.join("i.sock");
        assert!(!hand_over(&sock, &[]), "nobody listens yet");
        let (tx, rx) = std::sync::mpsc::channel();
        listen(sock.clone(), move |urls| {
            let _ = tx.send(urls);
        });
        assert!(hand_over(&sock, &["file:///tmp/a.sql".into(), "http://x".into()]));
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), vec!["file:///tmp/a.sql".to_string()]);
        assert!(hand_over(&sock, &[]));
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
