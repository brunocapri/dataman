//! One Dataman per user on Linux, as macOS keeps one per app: a second launch hands its files to
//! the first over an owner-only socket and exits. GPUI does not deliver files opened with the app
//! there, so they come from the command line.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use dataman_core::Dirs;

/// `file://` URLs for the paths on the command line (what `on_open_urls` would bring).
pub fn urls_from_args(args: &[String]) -> Vec<String> {
    args.iter()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .filter_map(|a| std::fs::canonicalize(a).ok())
        .filter(|p| p.is_file())
        .map(|p| file_url(&p))
        .collect()
}

pub fn file_url(p: &Path) -> String {
    let mut out = String::from("file://");
    for b in p.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

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
    let Ok(mut s) = UnixStream::connect(socket) else { return false };
    let mut msg = urls.join("\n");
    msg.push_str("\n\n");
    s.write_all(msg.as_bytes()).is_ok()
}

/// Listens for later launches; each sends its URLs (an empty list: show the window).
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
            let mut urls = Vec::new();
            for line in BufReader::new(s).lines().map_while(Result::ok) {
                if line.is_empty() {
                    break;
                }
                if line.starts_with("file://") && urls.len() < 64 {
                    urls.push(line);
                }
            }
            on(urls);
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
