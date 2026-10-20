//! Only the DMG is signed, not the feed: the version read from the DMG's own Info.plist must
//! match the feed's and be newer than the running one, so a replayed old DMG cannot downgrade
//! the app. The signature covers the whole file, like Sparkle's `sign_update`, so either tool
//! can sign. Copying goes through `ditto`, which keeps signatures and attributes.

use std::ffi::CString;
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
    pub url: String,
    pub size: u64,
    pub ed25519: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_macos: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
}

#[derive(Clone, Debug)]
pub struct UpdateConfig {
    pub feed: String,
    pub key: [u8; 32],
    pub current: String,
}

impl UpdateConfig {
    pub fn new(feed: Option<&str>, key_b64: Option<&str>, current: &str) -> Option<UpdateConfig> {
        let feed = feed.map(str::trim).filter(|f| !f.is_empty())?;
        let key = decode_key(key_b64?).map_err(|e| log::warn!("update: {e:#}")).ok()?;
        Some(UpdateConfig { feed: feed.to_string(), key, current: current.to_string() })
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

fn curl() -> Command {
    let mut c = Command::new("/usr/bin/curl");
    c.args(["--fail", "--silent", "--show-error", "--location", "--proto", "=https,http,file", "--connect-timeout", "15"]);
    c.stdin(Stdio::null());
    c
}

fn output(mut cmd: Command, what: &str) -> Result<Vec<u8>> {
    let out = cmd.stderr(Stdio::piped()).stdout(Stdio::piped()).output().with_context(|| format!("{what}: cannot run"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("{what}: {}", err.trim().lines().last().unwrap_or("failed"));
    }
    Ok(out.stdout)
}

pub fn check(cfg: &UpdateConfig) -> Result<Option<Feed>> {
    let mut c = curl();
    c.args(["--max-time", "30", "--max-filesize", &FEED_MAX.to_string(), &cfg.feed]);
    let body = output(c, "feed")?;
    let feed: Feed = serde_json::from_slice(&body).context("feed is not valid JSON")?;
    if version(&feed.version).is_none() {
        bail!("feed version {:?} is not a version", feed.version);
    }
    if !is_newer(&feed.version, &cfg.current) {
        return Ok(None);
    }
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
    let name = format!("Dataman-{}.dmg", sanitize(&feed.version));
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

fn writable(p: &Path) -> bool {
    let Ok(c) = CString::new(p.as_os_str().as_bytes()) else { return false };
    // SAFETY: a valid NUL-terminated path.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

/// A quarantined app run from where it was downloaded is translocated: it runs from a random
/// read-only mount.
pub fn check_writable(app: &Path) -> Result<()> {
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

pub fn plist_value(app: &Path, key: &str) -> Result<String> {
    let mut c = Command::new("/usr/bin/plutil");
    c.args(["-extract", key, "raw", "-o", "-"]).arg(app.join("Contents/Info.plist"));
    Ok(String::from_utf8_lossy(&output(c, "plutil")?).trim().to_string())
}

struct Mount(PathBuf);

impl Drop for Mount {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/hdiutil").args(["detach", "-quiet", "-force"]).arg(&self.0).stdout(Stdio::null()).stderr(Stdio::null()).status();
        let _ = std::fs::remove_dir(&self.0);
    }
}

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
        swap(&staged, app)?;
        log::info!("update: {} swapped in", app.display());
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&staged);
    result
}

fn swap(a: &Path, b: &Path) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

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
        assert!(macos_version().is_some_and(|v| version(&v).is_some()));
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
}
