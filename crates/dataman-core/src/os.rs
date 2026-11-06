use std::path::PathBuf;

pub const NAME: &str = if cfg!(target_os = "macos") {
    "macOS"
} else if cfg!(target_os = "linux") {
    "Linux"
} else if cfg!(windows) {
    "Windows"
} else {
    std::env::consts::OS
};

pub const LINUX: bool = cfg!(target_os = "linux");
pub const THIS_MACHINE: &str = if cfg!(target_os = "macos") { "this Mac" } else { "this computer" };
pub const WINDOWS: bool = cfg!(windows);

#[cfg(target_os = "macos")]
pub fn version() -> Option<String> {
    crate::update::macos_version()
}

#[cfg(windows)]
pub fn version() -> Option<String> {
    windows_version()
}

/// "Windows 11 Pro 24H2 (26100)", from the registry (what `winver` shows).
#[cfg(windows)]
fn windows_version() -> Option<String> {
    let key = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let product = reg_string(key, "ProductName")?;
    let build: u32 = reg_string(key, "CurrentBuildNumber").and_then(|b| b.parse().ok()).unwrap_or(0);
    // Windows 11 still says "Windows 10" in ProductName; its builds start at 22000.
    let product = if build >= 22000 { product.replacen("Windows 10", "Windows 11", 1) } else { product };
    Some(match reg_string(key, "DisplayVersion") {
        Some(v) => format!("{product} {v} ({build})"),
        None => format!("{product} ({build})"),
    })
}

#[cfg(windows)]
fn reg_string(key: &str, value: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (key, value) = (wide(key), wide(value));
    let mut buf = [0u16; 256];
    let mut len = (buf.len() * 2) as u32;
    let rc = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut len) };
    if rc != 0 {
        return None;
    }
    let n = (len as usize / 2).saturating_sub(1).min(buf.len());
    let s = String::from_utf16_lossy(&buf[..n]);
    (!s.trim().is_empty()).then(|| s.trim().to_string())
}

#[cfg(target_os = "linux")]
pub fn version() -> Option<String> {
    let text = std::fs::read_to_string("/etc/os-release").or_else(|_| std::fs::read_to_string("/usr/lib/os-release")).ok()?;
    os_release(&text, "PRETTY_NAME").or_else(|| os_release(&text, "NAME"))
}

pub fn os_release(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let v = l.strip_prefix(key)?.strip_prefix('=')?.trim();
        let v = v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(v);
        (!v.is_empty()).then(|| v.to_string())
    })
}

/// `HOME`; on Windows `USERPROFILE` (HOME only when a shell like Git Bash set it).
pub fn home() -> Option<PathBuf> {
    let var = |v: &str| std::env::var_os(v).filter(|h| !h.is_empty()).map(PathBuf::from);
    if WINDOWS { var("USERPROFILE").or_else(|| var("HOME")) } else { var("HOME") }
}

/// Windows' per-user folders: `%APPDATA%` (settings, roaming) and `%LOCALAPPDATA%` (this
/// machine's: logs, caches). Absolute paths only, as XDG's.
pub fn app_data() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(PathBuf::from).filter(|p| p.is_absolute())
}

pub fn local_app_data() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).filter(|p| p.is_absolute()).or_else(|| home().map(|h| h.join("AppData").join("Local")))
}

fn xdg(var: &str, fallback: &str) -> Option<PathBuf> {
    match std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from) {
        Some(p) if p.is_absolute() => Some(p),
        _ => home().map(|h| h.join(fallback)),
    }
}

pub fn state_home() -> Option<PathBuf> {
    xdg("XDG_STATE_HOME", ".local/state")
}

pub fn config_home() -> Option<PathBuf> {
    xdg("XDG_CONFIG_HOME", ".config")
}

/// `XDG_DOWNLOAD_DIR` from user-dirs.dirs, else ~/Downloads when it exists, else the home folder.
pub fn downloads_dir() -> Option<PathBuf> {
    let home = home()?;
    if LINUX
        && let Some(dir) =
            config_home().and_then(|c| std::fs::read_to_string(c.join("user-dirs.dirs")).ok()).and_then(|t| user_dir(&t, "XDG_DOWNLOAD_DIR", &home))
    {
        return Some(dir);
    }
    let d = home.join("Downloads");
    Some(if !LINUX || d.is_dir() { d } else { home })
}

/// `fs::canonicalize`, without the `\\?\` Windows puts before a drive path: the path as the
/// user and other programs write it (and as `strip_prefix` compares it with theirs).
pub fn canonical(path: impl AsRef<std::path::Path>) -> std::io::Result<PathBuf> {
    let p = std::fs::canonicalize(path)?;
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\")
            && rest.as_bytes().get(1) == Some(&b':')
            && rest.len() < 260
        {
            return Ok(PathBuf::from(rest));
        }
    }
    Ok(p)
}

/// A new file only this user can read (0600 on Unix; on Windows the folders Dataman writes in
/// are the user's own); an existing one is an error.
/// Replaces `path` whole or not at all: a temporary file of its own name (unique across
/// processes and threads) in the same folder, synced, renamed over it, and the folder synced;
/// the temporary goes on any failure. `private`: 0600 (this user's only).
pub fn atomic_write(path: &std::path::Path, bytes: &[u8], private: bool) -> std::io::Result<()> {
    use std::io::Write as _;
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.{}", std::process::id(), uuid::Uuid::new_v4().simple()));
    let r = (|| {
        let mut f = if private { private_new(&tmp)? } else { std::fs::File::options().write(true).create_new(true).open(&tmp)? };
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        #[cfg(unix)]
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

pub fn private_new(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::File::options();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)
}

pub fn user_dir(text: &str, key: &str, home: &std::path::Path) -> Option<PathBuf> {
    let v = text.lines().map(str::trim).filter(|l| !l.starts_with('#')).find_map(|l| l.strip_prefix(key)?.trim_start().strip_prefix('='))?.trim();
    let v = v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(v);
    let p = match v.strip_prefix("$HOME") {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(v),
    };
    (p.is_absolute() && p != home).then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_values() {
        let t = "NAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\nPRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nID=ubuntu\n";
        assert_eq!(os_release(t, "PRETTY_NAME").as_deref(), Some("Ubuntu 24.04.1 LTS"));
        assert_eq!(os_release(t, "ID").as_deref(), Some("ubuntu"));
        assert_eq!(os_release(t, "NAM"), None);
        assert_eq!(os_release("", "NAME"), None);
    }

    #[test]
    #[cfg(unix)]
    fn xdg_user_dirs() {
        use std::path::Path;
        let home = Path::new("/home/u");
        let t = "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOWNLOAD_DIR=\"$HOME/Téléchargements\"\n";
        assert_eq!(user_dir(t, "XDG_DOWNLOAD_DIR", home), Some(PathBuf::from("/home/u/Téléchargements")));
        assert_eq!(user_dir("XDG_DOWNLOAD_DIR=\"/data/dl\"", "XDG_DOWNLOAD_DIR", home), Some(PathBuf::from("/data/dl")));
        assert_eq!(user_dir("XDG_DOWNLOAD_DIR=\"$HOME/\"", "XDG_DOWNLOAD_DIR", home), None, "the home folder itself means unset");
        assert_eq!(user_dir("XDG_DOWNLOAD_DIR=\"relative\"", "XDG_DOWNLOAD_DIR", home), None);
        assert_eq!(user_dir(t, "XDG_MUSIC_DIR", home), None);
    }
}
