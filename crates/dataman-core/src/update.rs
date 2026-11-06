//! Only the download is signed, not the feed: the version read from the download itself (the
//! DMG's Info.plist, the AppImage's `--version`, the Windows installer's version resource) must match the feed's and be newer than the
//! running one, so a replayed old download cannot downgrade the app. The signature covers the
//! whole file, like Sparkle's `sign_update`, so either tool can sign. Copying a bundle goes
//! through `ditto`, which keeps signatures and attributes.
//!
//! The feed's top-level download is the macOS one (the only one older Macs read); `assets`
//! holds every platform's, keyed `<os>-<arch>` (`macos-aarch64`, `linux-x86_64`,
//! `windows-x86_64`: the installer, `xtask feed --asset windows-x86_64 <setup.exe> <url>`).
//! On Windows the installer is the update: run silently once the app has quit, for the same user
//! or machine and into the same folder as before, then the new copy is started. A portable copy
//! (the .zip) only announces a newer one. On Linux the AppImage reads `linux-<arch>`; an app the
//! .deb installed reads `linux-<arch>-deb`, and its update is installed by apt (or dpkg) through
//! pkexec, the system's own password prompt.

use std::collections::BTreeMap;
#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

const FEED_MAX: u64 = 1 << 20;
const DMG_MAX: u64 = 1 << 30;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feed {
    pub version: String,
    #[serde(default)]
    pub notes: String,
    // The top-level download may be missing (a release with no Mac build): defaults, so a
    // Linux client still reads the feed.
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub ed25519: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_macos: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub assets: BTreeMap<String, Asset>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub url: String,
    pub size: u64,
    pub ed25519: String,
}

/// This build's key in the feed's `assets`.
pub fn platform() -> String {
    let os = if cfg!(target_os = "macos") { "macos" } else { std::env::consts::OS };
    format!("{os}-{}", std::env::consts::ARCH)
}

/// The asset an install at `app` reads: `linux-<arch>-deb` when dpkg installed it.
pub fn asset_for(app: Option<&Path>) -> String {
    #[cfg(target_os = "linux")]
    if app.and_then(deb_package).is_some() {
        return format!("{}-deb", platform());
    }
    let _ = app;
    platform()
}

/// The package dpkg installed `file` from (`dpkg-query -S`), if any.
#[cfg(target_os = "linux")]
pub fn deb_package(file: &Path) -> Option<String> {
    let out = Command::new("dpkg-query").arg("-S").arg(file).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).lines().next()?.to_string();
    // "dataman: /usr/bin/dataman", or "dataman:amd64: …" for a multi-arch package.
    let name = line.split(": ").next()?.split(':').next()?.trim();
    (!name.is_empty() && !name.contains(',')).then(|| name.to_string())
}

impl Feed {
    /// The feed with this platform's download in its top-level fields, or `None` when the
    /// release has none for it.
    pub fn for_platform(mut self, platform: &str) -> Option<Feed> {
        if let Some(a) = self.assets.get(platform).cloned() {
            (self.url, self.size, self.ed25519) = (a.url, a.size, a.ed25519);
            return Some(self);
        }
        (platform.starts_with("macos-") && !self.url.is_empty()).then_some(self)
    }
}

/// Where the running app is installed, to be replaced by an update; else why it cannot be.
pub fn installed_app() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    if cfg!(target_os = "macos") {
        return bundle_of(&exe).ok_or_else(|| "Not running from Dataman.app".to_string());
    }
    // Only a copy the installer put there (its uninstall entry names this folder) updates itself.
    #[cfg(windows)]
    return match exe.parent().and_then(installed_scope) {
        Some(_) => Ok(exe),
        None => Err("A portable copy: download the new version from the release page".into()),
    };
    #[cfg(not(windows))]
    return match std::env::var_os("APPIMAGE").map(PathBuf::from) {
        Some(img) if img.is_file() => Ok(img),
        #[cfg(target_os = "linux")]
        _ if deb_package(&exe).is_some() => Ok(exe),
        _ if exe.starts_with("/usr/") || exe.starts_with("/opt/") => Err("Installed by a package manager: updates come with it".into()),
        _ => Err("Not running from the AppImage".into()),
    };
}

#[derive(Clone, Debug)]
pub struct UpdateConfig {
    pub feed: String,
    pub key: [u8; 32],
    pub current: String,
    /// The feed asset to read, when not this app's own (the update check's stand-in).
    pub asset: Option<String>,
}

impl UpdateConfig {
    pub fn new(feed: Option<&str>, key_b64: Option<&str>, current: &str) -> Option<UpdateConfig> {
        let feed = feed.map(str::trim).filter(|f| !f.is_empty())?;
        let key = decode_key(key_b64?).map_err(|e| log::warn!("update: {e:#}")).ok()?;
        Some(UpdateConfig { feed: feed.to_string(), key, current: current.to_string(), asset: None })
    }

    /// The feed asset this install reads (asks dpkg on Linux: off the UI thread).
    pub fn asset(&self) -> String {
        self.asset.clone().unwrap_or_else(|| asset_for(std::env::current_exe().ok().as_deref()))
    }
}

pub fn decode_key(b64: &str) -> Result<[u8; 32]> {
    let raw = data_encoding::BASE64.decode(b64.trim().as_bytes()).context("public key is not base64")?;
    raw.try_into().map_err(|_| anyhow!("public key is not 32 bytes"))
}

fn version(v: &str) -> Option<semver::Version> {
    let v = v.trim().trim_start_matches('v');
    semver::Version::parse(v).ok().or_else(|| {
        let mut parts: Vec<&str> = v.split('.').collect();
        while parts.len() < 3 {
            parts.push("0");
        }
        semver::Version::parse(&parts.join(".")).ok()
    })
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (version(candidate), version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

#[cfg(not(target_os = "macos"))]
pub fn macos_version() -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
pub fn macos_version() -> Option<String> {
    let name = CString::new("kern.osproductversion").ok()?;
    let mut buf = [0u8; 32];
    let mut len = buf.len();
    // SAFETY: the buffer and its length are valid for the call; no new value is set.
    let rc = unsafe { libc::sysctlbyname(name.as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
    if rc != 0 || len == 0 {
        return None;
    }
    let s = &buf[..len.min(buf.len())];
    let s = s.split(|b| *b == 0).next()?;
    Some(String::from_utf8_lossy(s).into_owned())
}

pub fn verify(bytes: &[u8], feed: &Feed, key: &[u8; 32]) -> Result<()> {
    if bytes.len() as u64 != feed.size {
        bail!("size mismatch: {} bytes, the feed says {}", bytes.len(), feed.size);
    }
    let sig = data_encoding::BASE64.decode(feed.ed25519.trim().as_bytes()).context("signature is not base64")?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(bytes, &sig)
        .map_err(|_| anyhow!("signature mismatch"))
}

#[cfg(target_os = "macos")]
fn curl() -> Command {
    let mut c = Command::new("/usr/bin/curl");
    c.args(["--fail", "--silent", "--show-error", "--location", "--proto", "=https,http,file", "--connect-timeout", "15"]);
    c.stdin(Stdio::null());
    c
}

#[cfg_attr(windows, allow(dead_code))]
fn output(mut cmd: Command, what: &str) -> Result<Vec<u8>> {
    let out = cmd.stderr(Stdio::piped()).stdout(Stdio::piped()).output().with_context(|| format!("{what}: cannot run"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("{what}: {}", err.trim().lines().last().unwrap_or("failed"));
    }
    Ok(out.stdout)
}

#[cfg(target_os = "macos")]
fn get(url: &str, max: u64) -> Result<Vec<u8>> {
    let mut c = curl();
    c.args(["--max-time", "30", "--max-filesize", &max.to_string(), url]);
    output(c, "feed")
}

#[cfg(not(target_os = "macos"))]
fn get(url: &str, max: u64) -> Result<Vec<u8>> {
    crate::http::get(url, max, "feed")
}

pub fn check(cfg: &UpdateConfig) -> Result<Option<Feed>> {
    let body = get(&cfg.feed, FEED_MAX)?;
    let feed: Feed = serde_json::from_slice(&body).context("feed is not valid JSON")?;
    if version(&feed.version).is_none() {
        bail!("feed version {:?} is not a version", feed.version);
    }
    if !is_newer(&feed.version, &cfg.current) {
        return Ok(None);
    }
    let offered = feed.version.clone();
    let asset = cfg.asset();
    let Some(feed) = feed.for_platform(&asset) else {
        log::info!("update: {offered} has no {asset} download");
        return Ok(None);
    };
    if let (Some(min), Some(os)) = (feed.minimum_macos.as_deref(), macos_version())
        && version(&os) < version(min)
    {
        log::info!("update: {} needs macOS {min}, this is {os}", feed.version);
        return Ok(None);
    }
    Ok(Some(feed))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Got(u64),
    Verifying,
}

#[cfg(not(target_os = "macos"))]
fn fetch_to(url: &str, size: u64, part: &Path, on: &dyn Fn(Phase)) -> Result<()> {
    crate::http::download(url, size, part, on)
}

#[cfg(target_os = "macos")]
fn fetch_to(url: &str, size: u64, part: &Path, on: &dyn Fn(Phase)) -> Result<()> {
    let mut c = curl();
    c.args(["--max-time", "1800", "--max-filesize", &size.to_string(), "--output"]).arg(part).arg(url);
    let mut child = c.stdout(Stdio::null()).stderr(Stdio::piped()).spawn().context("download: cannot run")?;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if let Ok(m) = std::fs::metadata(part) {
            on(Phase::Got(m.len()));
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    };
    if !status.success() {
        let mut err = String::new();
        if let Some(mut s) = child.stderr.take() {
            use std::io::Read as _;
            let _ = s.read_to_string(&mut err);
        }
        bail!("download: {}", err.trim().lines().last().unwrap_or("failed"));
    }
    on(Phase::Got(std::fs::metadata(part).map(|m| m.len()).unwrap_or(size)));
    Ok(())
}

pub fn download(cfg: &UpdateConfig, feed: &Feed, dir: &Path, on: &dyn Fn(Phase)) -> Result<PathBuf> {
    if feed.size == 0 || feed.size > DMG_MAX {
        bail!("implausible size {}", feed.size);
    }
    std::fs::create_dir_all(dir)?;
    let v = sanitize(&feed.version);
    let name = if cfg!(target_os = "macos") {
        format!("Dataman-{v}.dmg")
    } else if cfg!(windows) {
        format!("Dataman-{v}-setup.exe")
    } else if cfg.asset().ends_with("-deb") {
        format!("Dataman-{v}.deb")
    } else {
        format!("Dataman-{v}.AppImage")
    };
    let dest = dir.join(&name);
    if let Ok(bytes) = std::fs::read(&dest) {
        on(Phase::Got(bytes.len() as u64));
        on(Phase::Verifying);
        if verify(&bytes, feed, &cfg.key).is_ok() {
            return Ok(dest);
        }
        let _ = std::fs::remove_file(&dest);
    }
    let part = dir.join(format!("{name}.part"));
    let _ = std::fs::remove_file(&part);
    if let Err(e) = fetch_to(&feed.url, feed.size, &part, on) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    on(Phase::Verifying);
    let bytes = std::fs::read(&part)?;
    if let Err(e) = verify(&bytes, feed, &cfg.key) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, &dest)?;
    Ok(dest)
}

/// Made for one run, never stored: the app's offline update check signs its own feed with it.
pub struct ThrowawayKey(ring::signature::Ed25519KeyPair);

impl ThrowawayKey {
    pub fn new() -> Result<ThrowawayKey> {
        use ring::signature::Ed25519KeyPair;
        let doc = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).map_err(|_| anyhow!("key generation failed"))?;
        Ok(ThrowawayKey(Ed25519KeyPair::from_pkcs8(doc.as_ref()).map_err(|_| anyhow!("bad key"))?))
    }

    pub fn public(&self) -> String {
        use ring::signature::KeyPair as _;
        data_encoding::BASE64.encode(self.0.public_key().as_ref())
    }

    pub fn sign(&self, bytes: &[u8]) -> String {
        data_encoding::BASE64.encode(self.0.sign(bytes).as_ref())
    }
}

fn sanitize(v: &str) -> String {
    v.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+') { c } else { '_' }).collect()
}

pub fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    (macos.file_name()? == "MacOS" && contents.file_name()? == "Contents" && app.extension()? == "app").then(|| app.to_path_buf())
}

#[cfg(windows)]
fn writable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| !m.permissions().readonly())
}

#[cfg(unix)]
fn writable(p: &Path) -> bool {
    let Ok(c) = CString::new(p.as_os_str().as_bytes()) else { return false };
    // SAFETY: a valid NUL-terminated path.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

/// A quarantined app run from where it was downloaded is translocated: it runs from a random
/// read-only mount.
pub fn check_writable(app: &Path) -> Result<()> {
    // A package's files are root's: apt writes them, through pkexec.
    #[cfg(target_os = "linux")]
    if deb_package(app).is_some() {
        return Ok(());
    }
    let s = app.to_string_lossy();
    if s.contains("/AppTranslocation/") {
        bail!("translocated by Gatekeeper: move Dataman to Applications and open it from there");
    }
    let parent = app.parent().context("the app has no folder")?;
    if !writable(parent) || !writable(app) {
        bail!("{} is not writable", parent.display());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn plist_value(app: &Path, key: &str) -> Result<String> {
    let mut c = Command::new("/usr/bin/plutil");
    c.args(["-extract", key, "raw", "-o", "-"]).arg(app.join("Contents/Info.plist"));
    Ok(String::from_utf8_lossy(&output(c, "plutil")?).trim().to_string())
}

#[cfg(target_os = "macos")]
struct Mount(PathBuf);

#[cfg(target_os = "macos")]
impl Drop for Mount {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/hdiutil").args(["detach", "-quiet", "-force"]).arg(&self.0).stdout(Stdio::null()).stderr(Stdio::null()).status();
        let _ = std::fs::remove_dir(&self.0);
    }
}

#[cfg(target_os = "macos")]
/// The copy is a hidden sibling (same volume, for the swap); nothing changes at `app` unless
/// every step before the swap succeeded.
pub fn install(dmg: &Path, app: &Path, want: &str) -> Result<()> {
    check_writable(app)?;
    let parent = app.parent().context("the app has no folder")?;
    let name = app.file_name().context("the app has no name")?.to_string_lossy().into_owned();
    let nonce = format!("{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    let mount = std::env::temp_dir().join(format!("dataman-mount-{nonce}"));
    std::fs::create_dir_all(&mount)?;
    let mut c = Command::new("/usr/bin/hdiutil");
    c.args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-noverify", "-mountpoint"]).arg(&mount).arg(dmg);
    output(c, "hdiutil attach")?;
    log::info!("update: mounted {}", dmg.display());
    let guard = Mount(mount.clone());
    let src = std::fs::read_dir(&mount)?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "app") && !p.is_symlink())
        .context("no .app in the DMG")?;
    let staged = parent.join(format!(".{name}.update-{nonce}"));
    let mut c = Command::new("/usr/bin/ditto");
    c.arg(&src).arg(&staged);
    let copied = output(c, "ditto");
    drop(guard);
    if let Err(e) = copied {
        let _ = std::fs::remove_dir_all(&staged);
        return Err(e);
    }
    let result = (|| -> Result<()> {
        let _ = Command::new("/usr/bin/xattr").args(["-dr", "com.apple.quarantine"]).arg(&staged).stderr(Stdio::null()).status();
        let got = plist_value(&staged, "CFBundleShortVersionString")?;
        if self::version(&got).is_none() || self::version(&got) != self::version(want) {
            bail!("the DMG holds version {got}, the feed says {want}");
        }
        if let (Ok(want), Ok(id)) = (plist_value(app, "CFBundleIdentifier"), plist_value(&staged, "CFBundleIdentifier"))
            && want != id
        {
            bail!("the DMG holds {id}, this app is {want}");
        }
        let mut c = Command::new("/usr/bin/codesign");
        c.args(["--verify", "--deep", "--strict"]).arg(&staged);
        output(c, "codesign")?;
        // Any valid signature passes --verify: a Developer ID app takes only its own team's.
        if let Some(team) = team_id(app) {
            let got = team_id(&staged);
            if got.as_deref() != Some(team.as_str()) {
                bail!("the DMG's app is signed by {}, this app by {team}", got.as_deref().unwrap_or("no team"));
            }
        }
        swap(&staged, app)?;
        log::info!("update: {} swapped in", app.display());
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&staged);
    result
}

#[cfg(target_os = "macos")]
fn team_of(codesign_info: &str) -> Option<String> {
    let id = codesign_info.lines().find_map(|l| l.trim().strip_prefix("TeamIdentifier="))?.trim();
    (!id.is_empty() && id != "not set").then(|| id.to_string())
}

/// The signing team (`codesign -dv`), `None` for an ad hoc or unsigned app.
#[cfg(target_os = "macos")]
fn team_id(app: &Path) -> Option<String> {
    let out = Command::new("/usr/bin/codesign").args(["-dv", "--verbose=2"]).arg(app).stdin(Stdio::null()).output().ok()?;
    team_of(&String::from_utf8_lossy(&out.stderr))
}

#[cfg(target_os = "macos")]
fn swap(a: &Path, b: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let ca = CString::new(a.as_os_str().as_bytes())?;
        let cb = CString::new(b.as_os_str().as_bytes())?;
        // SAFETY: valid NUL-terminated paths.
        if unsafe { libc::renamex_np(ca.as_ptr(), cb.as_ptr(), libc::RENAME_SWAP) } == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        // Not APFS (HFS+, a network volume): two renames, the old app kept until the second one.
        if err.raw_os_error() != Some(libc::ENOTSUP) && err.raw_os_error() != Some(libc::EINVAL) {
            return Err(err).context("swap");
        }
    }
    let old = b.with_extension("app.old");
    let _ = std::fs::remove_dir_all(&old);
    std::fs::rename(b, &old).context("moving the old app aside")?;
    if let Err(e) = std::fs::rename(a, b) {
        let _ = std::fs::rename(&old, b);
        return Err(e).context("moving the new app in");
    }
    std::fs::rename(&old, a).context("moving the old app out")?;
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn relaunch_after_exit(app: &Path) -> Result<()> {
    let mut args: Vec<String> = Vec::new();
    for var in ["XDG_CONFIG_HOME", "DATAMAN_LOG_DIR"] {
        if let Some(v) = std::env::var_os(var) {
            args.push("--env".into());
            args.push(format!("{var}={}", v.to_string_lossy()));
        }
    }
    Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"pid=$1; app=$2; shift 2; while kill -0 "$pid" 2>/dev/null; do sleep 0.1; done; exec /usr/bin/open "$@" "$app""#)
        .arg("sh")
        .arg(std::process::id().to_string())
        .arg(app)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("relaunch")?;
    Ok(())
}

/// The AppImage is one file: the new one is staged beside it (same filesystem), its own
/// `--version` must say the feed's version, then one rename replaces the old. The running copy
/// keeps its mount of the old file. An app the .deb installed is updated by its package.
#[cfg(target_os = "linux")]
pub fn install(download: &Path, app: &Path, want: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    if let Some(package) = deb_package(app) {
        return install_deb(download, &package, want);
    }
    check_writable(app)?;
    let parent = app.parent().context("the app has no folder")?;
    let name = app.file_name().context("the app has no name")?.to_string_lossy().into_owned();
    let nonce = format!("{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    let staged = parent.join(format!(".{name}.update-{nonce}"));
    let result = (|| -> Result<()> {
        std::fs::copy(download, &staged).context("staging the update")?;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        let got = artifact_version(&staged)?;
        if self::version(&got).is_none() || self::version(&got) != self::version(want) {
            bail!("the download holds version {got}, the feed says {want}");
        }
        std::fs::rename(&staged, app).context("moving the new AppImage in")?;
        log::info!("update: {} replaced", app.display());
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

#[cfg(target_os = "linux")]
static ELEVATE: std::sync::Mutex<Option<Vec<String>>> = std::sync::Mutex::new(None);

/// What runs the package tool as root in place of pkexec: the update check's `sudo -n` in CI.
#[cfg(target_os = "linux")]
pub fn elevate_with(cmd: Option<&[&str]>) {
    *ELEVATE.lock().unwrap_or_else(|e| e.into_inner()) = cmd.map(|c| c.iter().map(|s| s.to_string()).collect());
}

/// The downloaded (and verified) .deb must be the installed package at the feed's version; apt
/// installs it (dpkg where there is no apt) as root through pkexec, which asks for the password
/// as the desktop does. With no pkexec or polkit agent, the error says the command to run.
#[cfg(target_os = "linux")]
fn install_deb(deb: &Path, package: &str, want: &str) -> Result<()> {
    let field = |f: &str| -> Result<String> {
        let mut c = Command::new("dpkg-deb");
        c.arg("--field").arg(deb).arg(f).stdin(Stdio::null());
        Ok(String::from_utf8_lossy(&output(c, "dpkg-deb")?).trim().to_string())
    };
    let (name, got) = (field("Package")?, field("Version")?);
    if name != package {
        bail!("the download is the package {name}, not {package}");
    }
    if self::version(&got).is_none() || self::version(&got) != self::version(want) {
        bail!("the download holds version {got}, the feed says {want}");
    }
    let by_hand = || anyhow!("Run: sudo apt install {}", deb.display());
    let elevate = ELEVATE.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(|| vec!["pkexec".into()]);
    let mut c = Command::new(&elevate[0]);
    c.args(&elevate[1..]);
    if Path::new("/usr/bin/apt-get").exists() {
        c.args(["/usr/bin/apt-get", "install", "-y"]);
    } else {
        c.args(["/usr/bin/dpkg", "-i"]);
    }
    let out = match c.arg(deb).env("DEBIAN_FRONTEND", "noninteractive").stdin(Stdio::null()).output() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(by_hand()),
        r => r.context("installing the package")?,
    };
    match out.status.code() {
        Some(0) => {}
        // pkexec: the password prompt was dismissed.
        Some(126) => bail!("Not installed: the password was not given"),
        // pkexec: no polkit agent to ask with, or not allowed.
        Some(127) => return Err(by_hand()),
        _ => {
            let err = String::from_utf8_lossy(&out.stderr);
            let line = err.lines().rev().find(|l| !l.trim().is_empty() && !l.starts_with("N: ") && !l.starts_with("W: ")).unwrap_or("failed");
            bail!("installing the package: {}", line.trim());
        }
    }
    let mut c = Command::new("dpkg-query");
    c.args(["--show", "--showformat=${Version}", package]).stdin(Stdio::null());
    let now = String::from_utf8_lossy(&output(c, "dpkg-query")?).trim().to_string();
    if self::version(&now) != self::version(want) {
        bail!("{package} is {now} after the install, not {want}");
    }
    log::info!("update: {package} {now} installed");
    Ok(())
}

/// What a downloaded (and verified) AppImage says it is; extracted rather than mounted, so no
/// FUSE is needed for it.
#[cfg(target_os = "linux")]
fn artifact_version(file: &Path) -> Result<String> {
    let mut child = Command::new(file)
        .arg("--version")
        .env("APPIMAGE_EXTRACT_AND_RUN", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("running the download")?;
    let start = std::time::Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if start.elapsed() > std::time::Duration::from_secs(60) {
            let _ = child.kill();
            bail!("the download did not say its version");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let mut out = String::new();
    if let Some(mut o) = child.stdout.take() {
        use std::io::Read as _;
        let _ = o.read_to_string(&mut out);
    }
    Ok(out.lines().next().unwrap_or("").trim().to_string())
}

/// The environment goes with the process (no LaunchServices in between, as on macOS).
#[cfg(target_os = "linux")]
pub fn relaunch_after_exit(app: &Path) -> Result<()> {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"pid=$1; app=$2; while kill -0 "$pid" 2>/dev/null; do sleep 0.1; done; exec "$app""#)
        .arg("sh")
        .arg(std::process::id().to_string())
        .arg(app)
        .env_remove("APPDIR")
        .env_remove("APPIMAGE")
        .env_remove("ARGV0")
        .env_remove("OWD")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("relaunch")?;
    Ok(())
}

/// Inno Setup's uninstall entry for Dataman: the AppId of packaging/windows/dataman.iss, `_is1`.
#[cfg(windows)]
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{C9AF68F1-847F-455B-9E7F-9AF048C6A989}_is1";

#[cfg(windows)]
fn reg_string(root: windows_sys::Win32::System::Registry::HKEY, key: &str, value: &str, flags: u32) -> Option<String> {
    use windows_sys::Win32::System::Registry::{RRF_RT_REG_SZ, RegGetValueW};
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (key, value) = (wide(key), wide(value));
    let mut buf = vec![0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: NUL-terminated names; the buffer's size in bytes is passed with it.
    let rc = unsafe { RegGetValueW(root, key.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ | flags, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut len) };
    if rc != 0 {
        return None;
    }
    let n = (len as usize / 2).saturating_sub(1).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]).trim().to_string())
}

/// Where the installer put Dataman, if in `dir`: `Some(false)` for this user, `Some(true)` for the
/// machine (its update asks for an administrator).
#[cfg(windows)]
fn installed_scope(dir: &Path) -> Option<bool> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_SUBKEY_WOW6464KEY};
    let here = dir.to_string_lossy().trim_end_matches('\\').to_lowercase();
    let at = |root, flags| reg_string(root, UNINSTALL_KEY, "InstallLocation", flags).is_some_and(|l| l.trim_end_matches('\\').to_lowercase() == here);
    if at(HKEY_CURRENT_USER, 0) {
        Some(false)
    } else if at(HKEY_LOCAL_MACHINE, RRF_SUBKEY_WOW6464KEY) {
        Some(true)
    } else {
        None
    }
}

/// An executable's version resource (major.minor.patch), as the installer's `VersionInfoVersion`
/// sets it.
#[cfg(windows)]
pub fn file_version(file: &Path) -> Result<String> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW};
    let path: Vec<u16> = file.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: a NUL-terminated path; the block is as large as the system asked for, and the fixed
    // info read from it lies inside it (its length checked).
    unsafe {
        let n = GetFileVersionInfoSizeW(path.as_ptr(), std::ptr::null_mut());
        if n == 0 {
            bail!("{} has no version: {}", file.display(), std::io::Error::last_os_error());
        }
        let mut block = vec![0u8; n as usize];
        if GetFileVersionInfoW(path.as_ptr(), 0, n, block.as_mut_ptr().cast()) == 0 {
            bail!("{}: {}", file.display(), std::io::Error::last_os_error());
        }
        let root = [u16::from(b'\\'), 0];
        let (mut info, mut len) = (std::ptr::null_mut(), 0u32);
        if VerQueryValueW(block.as_ptr().cast(), root.as_ptr(), &mut info, &mut len) == 0 || (len as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>() {
            bail!("{} has no version", file.display());
        }
        let f = &*(info as *const VS_FIXEDFILEINFO);
        Ok(format!("{}.{}.{}", f.dwFileVersionMS >> 16, f.dwFileVersionMS & 0xffff, f.dwFileVersionLS >> 16))
    }
}

/// The installer as an update runs it: no window, no question, no restart; the apps holding its
/// files (an agent's `dataman-mcp`) closed; the scope and folder of the copy it replaces.
#[cfg(windows)]
fn installer_args(dir: &Path, all_users: bool, log: &Path) -> String {
    let scope = if all_users { "/ALLUSERS" } else { "/CURRENTUSER" };
    format!(r#"/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS {scope} /DIR="{}" /LOG="{}""#, dir.display(), log.display())
}

/// The installer that runs once the app has quit, with its arguments.
#[cfg(windows)]
static PENDING: std::sync::Mutex<Option<(PathBuf, String)>> = std::sync::Mutex::new(None);

/// The download is the installer: its own version must be the feed's. The running copy's files are
/// in use, so for it the installer waits for the app to quit (`relaunch_after_exit`); another copy
/// (the update check's) is installed now, and must then say the new version.
#[cfg(windows)]
pub fn install(download: &Path, app: &Path, want: &str) -> Result<()> {
    use std::os::windows::process::CommandExt as _;
    let got = file_version(download)?;
    if self::version(&got).is_none() || self::version(&got) != self::version(want) {
        bail!("the download holds version {got}, the feed says {want}");
    }
    let dir = app.parent().context("the app has no folder")?;
    let nonce = format!("{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    let stage = std::env::temp_dir().join(format!("dataman-update-{nonce}"));
    std::fs::create_dir_all(&stage)?;
    let setup = stage.join(download.file_name().context("the download has no name")?);
    std::fs::copy(download, &setup).context("staging the installer")?;
    let log = stage.join("install.log");
    let args = installer_args(dir, installed_scope(dir).unwrap_or(false), &log);
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if std::env::current_exe().is_ok_and(|exe| canon(&exe) == canon(app)) {
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some((setup, args));
        log::info!("update: the installer runs once Dataman has quit");
        return Ok(());
    }
    let status = Command::new(&setup).raw_arg(&args).status().context("running the installer")?;
    if !status.success() {
        let tail = std::fs::read_to_string(&log).ok().and_then(|l| l.lines().rev().find(|l| !l.trim().is_empty()).map(str::to_string)).unwrap_or_default();
        bail!("the installer failed ({status}): {tail}");
    }
    let _ = std::fs::remove_dir_all(&stage);
    let now = file_version(app)?;
    if self::version(&now) != self::version(want) {
        bail!("installed, but {} says {now}", app.display());
    }
    log::info!("update: {} installed", app.display());
    Ok(())
}

/// With an installer waiting (`install`), it runs once this process has exited and the app
/// starts after it; else the app starts again at once.
#[cfg(windows)]
pub fn relaunch_after_exit(app: &Path) -> Result<()> {
    match PENDING.lock().unwrap_or_else(|e| e.into_inner()).take() {
        Some((setup, args)) => apply_after_exit(std::process::id(), &setup, &args, app).map(drop),
        None => {
            Command::new(app).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().context("relaunch")?;
            Ok(())
        }
    }
}

/// Once `pid` has exited: the installer with `args`, waited for, then `app` (the new copy, or the
/// old one if the installer failed). The installer's folder goes when it succeeded; its log stays
/// when not. A hidden PowerShell, which outlives the app.
#[cfg(windows)]
pub fn apply_after_exit(pid: u32, setup: &Path, args: &str, app: &Path) -> Result<std::process::Child> {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const SCRIPT: &str = "Wait-Process -Id $env:DATAMAN_PID -ErrorAction SilentlyContinue; \
        $p = Start-Process -FilePath $env:DATAMAN_SETUP -ArgumentList $env:DATAMAN_ARGS -Wait -PassThru; \
        Start-Process -FilePath $env:DATAMAN_APP; \
        if ($p.ExitCode -eq 0) { Remove-Item -LiteralPath (Split-Path -Parent $env:DATAMAN_SETUP) -Recurse -Force -ErrorAction SilentlyContinue }";
    Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-Command", SCRIPT])
        .env("DATAMAN_PID", pid.to_string())
        .env("DATAMAN_SETUP", setup)
        .env("DATAMAN_ARGS", args)
        .env("DATAMAN_APP", app)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting the installer's helper")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    #[test]
    fn feeds_name_each_platform() {
        let old = r#"{"version":"0.4.0","url":"https://x/Dataman-0.4.0.dmg","size":10,"ed25519":"m"}"#;
        let f: Feed = serde_json::from_str(old).unwrap();
        assert!(f.assets.is_empty());
        assert_eq!(f.clone().for_platform("macos-aarch64").map(|f| f.url), Some("https://x/Dataman-0.4.0.dmg".into()), "an old feed is the Mac's");
        assert_eq!(f.for_platform("linux-x86_64"), None);
        let new = r#"{"version":"0.4.0","url":"https://x/Dataman-0.4.0.dmg","size":10,"ed25519":"m",
            "assets":{"macos-aarch64":{"url":"https://x/Dataman-0.4.0.dmg","size":10,"ed25519":"m"},
                      "linux-x86_64":{"url":"https://x/Dataman-x86_64.AppImage","size":20,"ed25519":"l"}}}"#;
        let f: Feed = serde_json::from_str(new).unwrap();
        let linux = f.clone().for_platform("linux-x86_64").unwrap();
        assert_eq!((linux.url.as_str(), linux.size, linux.ed25519.as_str(), linux.version.as_str()), ("https://x/Dataman-x86_64.AppImage", 20, "l", "0.4.0"));
        assert_eq!(f.clone().for_platform("linux-aarch64"), None);
        assert_eq!(f.clone().for_platform("windows-x86_64"), None, "never the Mac's or Linux's download on Windows");
        let with_windows = r#"{"version":"0.4.0","assets":{"linux-x86_64":{"url":"https://x/a.AppImage","size":3,"ed25519":"s"},
            "windows-x86_64":{"url":"https://x/Dataman-0.4.0-windows-x86_64-setup.exe","size":4,"ed25519":"w"}}}"#;
        let w: Feed = serde_json::from_str(with_windows).unwrap();
        assert_eq!(w.clone().for_platform("windows-x86_64").map(|f| f.url), Some("https://x/Dataman-0.4.0-windows-x86_64-setup.exe".into()));
        assert_eq!(w.for_platform("windows-aarch64"), None);
        assert_eq!(f.for_platform("macos-aarch64").map(|f| f.size), Some(10));
        let linux_only = r#"{"version":"0.4.0","assets":{"linux-aarch64":{"url":"https://x/a.AppImage","size":3,"ed25519":"s"}}}"#;
        let f: Feed = serde_json::from_str(linux_only).unwrap();
        assert_eq!(f.clone().for_platform("linux-aarch64").map(|f| f.url), Some("https://x/a.AppImage".into()));
        assert_eq!(f.for_platform("macos-aarch64"), None, "no Mac download, no update for Macs");
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct Shipped {
            version: String,
            url: String,
            size: u64,
            ed25519: String,
            minimum_macos: Option<String>,
        }
        let shipped: Shipped = serde_json::from_str(new).unwrap();
        assert_eq!((shipped.url.as_str(), shipped.size), ("https://x/Dataman-0.4.0.dmg", 10), "what released Macs read is unchanged");
        assert!(platform().ends_with(std::env::consts::ARCH));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn an_appimage_update_replaces_the_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("dataman-appimage-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Apps")).unwrap();
        let stand_in = |path: &Path, v: &str| crate::cloudauth::write_script(path, &format!("echo {v}"));
        let app = dir.join("Apps/Dataman.AppImage");
        stand_in(&app, "0.1.0");
        let new = dir.join("new.AppImage");
        stand_in(&new, "0.2.0");
        let err = install(&new, &app, "0.3.0").unwrap_err();
        assert!(err.to_string().contains("holds version 0.2.0"), "{err:#}");
        assert_eq!(artifact_version(&app).unwrap(), "0.1.0", "untouched after a refusal");
        install(&new, &app, "0.2.0").unwrap();
        assert_eq!(artifact_version(&app).unwrap(), "0.2.0");
        assert!(std::fs::metadata(&app).unwrap().permissions().mode() & 0o111 != 0);
        let left: Vec<_> = std::fs::read_dir(dir.join("Apps")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, vec![std::ffi::OsString::from("Dataman.AppImage")], "nothing staged left");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn signing_team() {
        assert_eq!(team_of("Executable=/A.app/Contents/MacOS/a\nAuthority=Developer ID Application: B (ABCDE12345)\nTeamIdentifier=ABCDE12345\n").as_deref(), Some("ABCDE12345"));
        assert_eq!(team_of("Signature=adhoc\nTeamIdentifier=not set\n"), None);
        assert_eq!(team_of("/x.app: code object is not signed at all\n"), None);
        assert_eq!(team_id(Path::new("/System/Applications/Calculator.app")), None, "Apple's own apps have no team");
    }

    fn keypair() -> Ed25519KeyPair {
        let doc = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        Ed25519KeyPair::from_pkcs8(doc.as_ref()).unwrap()
    }

    #[test]
    fn versions() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.2", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0-beta.1", "0.1.0"));
        assert!(is_newer("0.1.0", "0.1.0-beta.1"));
        assert!(!is_newer("garbage", "0.1.0"));
        assert_eq!(macos_version().is_some_and(|v| version(&v).is_some()), cfg!(target_os = "macos"));
    }

    #[test]
    fn config_needs_feed_and_key() {
        let kp = keypair();
        let key = data_encoding::BASE64.encode(kp.public_key().as_ref());
        assert!(UpdateConfig::new(None, Some(&key), "0.1.0").is_none());
        assert!(UpdateConfig::new(Some(" "), Some(&key), "0.1.0").is_none());
        assert!(UpdateConfig::new(Some("https://x/feed.json"), None, "0.1.0").is_none());
        assert!(UpdateConfig::new(Some("https://x/feed.json"), Some("AAAA"), "0.1.0").is_none());
        assert!(UpdateConfig::new(Some("https://x/feed.json"), Some(&key), "0.1.0").is_some());
    }

    #[test]
    fn signature_and_size() {
        let kp = keypair();
        let key: [u8; 32] = kp.public_key().as_ref().try_into().unwrap();
        let dmg = b"not really a dmg, but bytes all the same".to_vec();
        let feed = Feed {
            version: "0.2.0".into(),
            notes: String::new(),
            url: "file:///x".into(),
            size: dmg.len() as u64,
            ed25519: data_encoding::BASE64.encode(kp.sign(&dmg).as_ref()),
            minimum_macos: None,
            published: None,
            assets: Default::default(),
        };
        verify(&dmg, &feed, &key).unwrap();
        let mut bad = dmg.clone();
        bad[3] ^= 1;
        assert!(verify(&bad, &feed, &key).unwrap_err().to_string().contains("signature"));
        assert!(verify(&dmg[1..], &feed, &key).unwrap_err().to_string().contains("size"));
        let other: [u8; 32] = keypair().public_key().as_ref().try_into().unwrap();
        assert!(verify(&dmg, &feed, &other).is_err());
        let json = serde_json::to_string(&feed).unwrap();
        assert_eq!(serde_json::from_str::<Feed>(&json).unwrap(), feed);
        let min: Feed = serde_json::from_str(r#"{"version":"1.0.0","url":"u","size":1,"ed25519":"x"}"#).unwrap();
        assert_eq!((min.notes.as_str(), min.minimum_macos), ("", None));
    }

    #[test]
    fn download_reports_progress() {
        let key = ThrowawayKey::new().unwrap();
        let dir = std::env::temp_dir().join(format!("dataman-update-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.dmg");
        let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &bytes).unwrap();
        let cfg = UpdateConfig::new(Some("file:///unused"), Some(&key.public()), "0.1.0").unwrap();
        let mut feed = Feed {
            version: "0.2.0".into(),
            notes: String::new(),
            url: format!("file://{}", src.display()),
            size: bytes.len() as u64,
            ed25519: key.sign(&bytes),
            minimum_macos: None,
            published: None,
            assets: Default::default(),
        };
        let seen = std::sync::Mutex::new(Vec::new());
        let out = download(&cfg, &feed, &dir.join("got"), &|p| seen.lock().unwrap().push(p)).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), bytes);
        let seen = seen.into_inner().unwrap();
        assert!(seen.contains(&Phase::Got(bytes.len() as u64)), "{seen:?}");
        assert_eq!(seen.last(), Some(&Phase::Verifying));
        feed.version = "0.3.0".into();
        feed.ed25519 = ThrowawayKey::new().unwrap().sign(&bytes);
        let err = download(&cfg, &feed, &dir.join("bad"), &|_| {}).unwrap_err();
        assert!(err.to_string().contains("signature"), "{err:#}");
        assert_eq!(std::fs::read_dir(dir.join("bad")).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bundle_paths() {
        assert_eq!(bundle_of(Path::new("/Applications/Dataman.app/Contents/MacOS/dataman")), Some(PathBuf::from("/Applications/Dataman.app")));
        assert_eq!(bundle_of(Path::new("/x/target/release/dataman")), None);
        assert!(check_writable(Path::new("/private/var/folders/x/AppTranslocation/ABC/d/Dataman.app")).unwrap_err().to_string().contains("translocated"));
        assert!(check_writable(Path::new("/System/Applications/Calculator.app")).is_err());
    }

    /// The installer waits for the app to have quit, gets its arguments as they are, and the app
    /// starts after it; the installer's folder goes once it succeeded.
    #[cfg(windows)]
    #[test]
    fn the_installer_runs_once_the_app_has_quit_and_the_app_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let stage = d.join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        let setup = stage.join("setup.cmd");
        std::fs::write(
            &setup,
            format!(
                "@echo off\r\ntasklist /FI \"PID eq %DATAMAN_PID%\" | find \"%DATAMAN_PID%\" >nul && echo early>\"{0}\\early.txt\"\r\necho %*>\"{0}\\setup.txt\"\r\n",
                d.display()
            ),
        )
        .unwrap();
        let app = d.join("app.cmd");
        std::fs::write(&app, format!("@echo started>\"{}\\app.txt\"\r\n", d.display())).unwrap();
        // The app that quits: about two seconds. Its handle closed at once, as the real app's is.
        let pid = Command::new("cmd").args(["/c", "ping -n 3 127.0.0.1 >nul"]).spawn().unwrap().id();
        let args = r#"/VERYSILENT /CURRENTUSER /DIR="C:\Some Folder""#;
        let mut helper = apply_after_exit(pid, &setup, args, &app).unwrap();
        assert!(helper.wait().unwrap().success());
        assert!(!d.join("early.txt").exists(), "the installer ran while the app was still up");
        assert_eq!(std::fs::read_to_string(d.join("setup.txt")).unwrap().trim(), args);
        let started = (0..100).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            d.join("app.txt").exists()
        });
        assert!(started, "the app did not start after the installer");
        assert!(!stage.exists(), "the installer's folder is left");
    }

    #[cfg(windows)]
    #[test]
    fn the_installer_gets_the_scope_and_folder() {
        let a = installer_args(Path::new(r"C:\Users\u\AppData\Local\Programs\Dataman"), false, Path::new(r"C:\t\install.log"));
        assert_eq!(a, r#"/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS /CURRENTUSER /DIR="C:\Users\u\AppData\Local\Programs\Dataman" /LOG="C:\t\install.log""#);
        assert!(installer_args(Path::new(r"C:\Program Files\Dataman"), true, Path::new("l")).contains("/ALLUSERS /DIR=\"C:\\Program Files\\Dataman\""));
    }
}
