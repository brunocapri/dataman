use std::path::PathBuf;

pub const NAME: &str = if cfg!(target_os = "macos") {
    "macOS"
} else if cfg!(target_os = "linux") {
    "Linux"
} else {
    std::env::consts::OS
};

pub const LINUX: bool = cfg!(target_os = "linux");

#[cfg(target_os = "macos")]
pub fn version() -> Option<String> {
    crate::update::macos_version()
}

#[cfg(not(target_os = "macos"))]
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

pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
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
        && let Some(dir) = config_home().and_then(|c| std::fs::read_to_string(c.join("user-dirs.dirs")).ok()).and_then(|t| user_dir(&t, "XDG_DOWNLOAD_DIR", &home))
    {
        return Some(dir);
    }
    let d = home.join("Downloads");
    Some(if !LINUX || d.is_dir() { d } else { home })
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
    use std::path::Path;

    #[test]
    fn os_release_values() {
        let t = "NAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\nPRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nID=ubuntu\n";
        assert_eq!(os_release(t, "PRETTY_NAME").as_deref(), Some("Ubuntu 24.04.1 LTS"));
        assert_eq!(os_release(t, "ID").as_deref(), Some("ubuntu"));
        assert_eq!(os_release(t, "NAM"), None);
        assert_eq!(os_release("", "NAME"), None);
    }

    #[test]
    fn xdg_user_dirs() {
        let home = Path::new("/home/u");
        let t = "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOWNLOAD_DIR=\"$HOME/Téléchargements\"\n";
        assert_eq!(user_dir(t, "XDG_DOWNLOAD_DIR", home), Some(PathBuf::from("/home/u/Téléchargements")));
        assert_eq!(user_dir("XDG_DOWNLOAD_DIR=\"/data/dl\"", "XDG_DOWNLOAD_DIR", home), Some(PathBuf::from("/data/dl")));
        assert_eq!(user_dir("XDG_DOWNLOAD_DIR=\"$HOME/\"", "XDG_DOWNLOAD_DIR", home), None, "the home folder itself means unset");
        assert_eq!(user_dir("XDG_DOWNLOAD_DIR=\"relative\"", "XDG_DOWNLOAD_DIR", home), None);
        assert_eq!(user_dir(t, "XDG_MUSIC_DIR", home), None);
    }
}
