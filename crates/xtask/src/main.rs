//! Diskman's release tooling (`cargo run --release -p xtask -- <command>`):
//!
//! - `icon <svg> <out.icns>`: renders the icon at every size macOS wants and packs it with
//!   `iconutil`.
//! - `keygen`: a new ed25519 key for update signatures, written outside the repository (0600);
//!   prints the public key for `DISKMAN_UPDATE_PUBKEY`.
//! - `pubkey`: prints the public key of the current private key.
//! - `sign <dmg> --url <url> --version <v> [--notes <file>] [--min-macos <v>]`: signs the DMG and
//!   prints the feed JSON.
//! - `verify <feed.json> <dmg> --pubkey <b64> [--version <v>]`: checks a published feed and its
//!   DMG as the app would (size, signature, version).
//!
//! The private key: `DISKMAN_UPDATE_PRIVATE_KEY` (its base64, for CI secrets), else the file
//! at `DISKMAN_UPDATE_KEY`, else `~/.config/diskman-release/update_ed25519.key`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, anyhow, bail};
use resvg::{tiny_skia, usvg};
use ring::signature::{Ed25519KeyPair, KeyPair};

fn main() {
    if let Err(e) = run() {
        eprintln!("xtask: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("icon") if args.len() == 3 => icon(Path::new(&args[1]), Path::new(&args[2])),
        Some("keygen") => keygen(args.iter().any(|a| a == "--force")),
        Some("pubkey") => {
            println!("{}", public_key(&load_key()?));
            Ok(())
        }
        Some("sign") if args.len() >= 2 => sign(&args[1], &args[2..]),
        Some("verify") if args.len() >= 3 => verify(&args[1], &args[2], &args[3..]),
        _ => bail!(
            "usage: xtask icon <svg> <out.icns> | keygen [--force] | pubkey | sign <dmg> --url <url> --version <v> [--notes <file>] [--min-macos <v>] | verify <feed.json> <dmg> --pubkey <b64> [--version <v>]"
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// The icon.

fn icon(svg: &Path, out: &Path) -> Result<()> {
    let data = std::fs::read(svg).with_context(|| format!("reading {}", svg.display()))?;
    let tree = usvg::Tree::from_data(&data, &usvg::Options::default())?;
    let set = std::env::temp_dir().join(format!("diskman-{}.iconset", std::process::id()));
    let _ = std::fs::remove_dir_all(&set);
    std::fs::create_dir_all(&set)?;
    for base in [16u32, 32, 128, 256, 512] {
        for (scale, suffix) in [(1u32, ""), (2, "@2x")] {
            let px = base * scale;
            let mut pixmap = tiny_skia::Pixmap::new(px, px).context("pixmap")?;
            let k = px as f32 / tree.size().width();
            resvg::render(&tree, tiny_skia::Transform::from_scale(k, k), &mut pixmap.as_mut());
            pixmap.save_png(set.join(format!("icon_{base}x{base}{suffix}.png")))?;
        }
    }
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let st = Command::new("iconutil").args(["-c", "icns", "-o"]).arg(out).arg(&set).status()?;
    // The 1024 px render, to look at.
    let preview = out.with_extension("png");
    std::fs::copy(set.join("icon_512x512@2x.png"), &preview)?;
    std::fs::remove_dir_all(&set)?;
    if !st.success() {
        bail!("iconutil failed");
    }
    println!("{} ({} bytes), preview {}", out.display(), std::fs::metadata(out)?.len(), preview.display());
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Update signing.

fn key_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("DISKMAN_UPDATE_KEY").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/diskman-release/update_ed25519.key"))
}

fn inside_git(dir: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .is_ok_and(|o| o.status.success() && o.stdout.starts_with(b"true"))
}

fn keygen(force: bool) -> Result<()> {
    let path = key_path()?;
    let dir = path.parent().context("key path has no folder")?;
    std::fs::create_dir_all(dir)?;
    if inside_git(dir) {
        bail!("{} is inside a git repository: keep the private key out of it (DISKMAN_UPDATE_KEY)", dir.display());
    }
    if path.exists() && !force {
        bail!("{} exists (--force replaces it; apps built with the old public key would refuse new updates)", path.display());
    }
    let doc = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).map_err(|_| anyhow!("key generation failed"))?;
    let kp = Ed25519KeyPair::from_pkcs8(doc.as_ref()).map_err(|_| anyhow!("bad key"))?;
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
        writeln!(f, "{}", data_encoding::BASE64.encode(doc.as_ref()))?;
    }
    eprintln!("private key: {} (0600; back it up, never commit it)", path.display());
    eprintln!("public key (build with DISKMAN_UPDATE_PUBKEY=…):");
    println!("{}", public_key(&kp));
    Ok(())
}

fn load_key() -> Result<Ed25519KeyPair> {
    let b64 = match std::env::var("DISKMAN_UPDATE_PRIVATE_KEY") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => {
            let p = key_path()?;
            std::fs::read_to_string(&p).with_context(|| format!("reading {} (make one with `xtask keygen`)", p.display()))?
        }
    };
    let der = data_encoding::BASE64.decode(b64.trim().as_bytes()).context("private key is not base64")?;
    Ed25519KeyPair::from_pkcs8(&der).map_err(|_| anyhow!("private key is not an ed25519 PKCS#8 key"))
}

fn public_key(kp: &Ed25519KeyPair) -> String {
    data_encoding::BASE64.encode(kp.public_key().as_ref())
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

/// UTC "YYYY-MM-DDTHH:MM:SSZ" (days to civil date, Howard Hinnant's algorithm).
fn now_utc() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn sign(dmg: &str, args: &[String]) -> Result<()> {
    let url = flag(args, "--url").context("--url is required")?;
    let version = flag(args, "--version").context("--version is required")?;
    let notes = match flag(args, "--notes") {
        Some(p) => std::fs::read_to_string(p).with_context(|| format!("reading {p}"))?.trim().to_string(),
        None => String::new(),
    };
    let kp = load_key()?;
    // Signed with the key the apps trust, or refuse.
    if let Ok(want) = std::env::var("DISKMAN_UPDATE_PUBKEY")
        && !want.trim().is_empty()
        && want.trim() != public_key(&kp)
    {
        bail!("the private key does not match DISKMAN_UPDATE_PUBKEY");
    }
    let bytes = std::fs::read(dmg).with_context(|| format!("reading {dmg}"))?;
    let sig = kp.sign(&bytes);
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, kp.public_key().as_ref())
        .verify(&bytes, sig.as_ref())
        .map_err(|_| anyhow!("self-check failed"))?;
    let mut feed = serde_json::json!({
        "version": version,
        "notes": notes,
        "url": url,
        "size": bytes.len(),
        "ed25519": data_encoding::BASE64.encode(sig.as_ref()),
        "published": now_utc(),
    });
    if let Some(min) = flag(args, "--min-macos") {
        feed["minimum_macos"] = min.into();
    }
    println!("{}", serde_json::to_string_pretty(&feed)?);
    Ok(())
}

/// Checks a published feed against its DMG the way the app does: the size, the ed25519
/// signature with the app's public key, and (with `--version`) the version.
fn verify(feed: &str, dmg: &str, args: &[String]) -> Result<()> {
    let pubkey = flag(args, "--pubkey").context("--pubkey is required")?;
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(feed).with_context(|| format!("reading {feed}"))?).context("the feed is not JSON")?;
    let version = json["version"].as_str().context("the feed has no version")?;
    if let Some(want) = flag(args, "--version")
        && want != version
    {
        bail!("the feed offers {version}, expected {want}");
    }
    let bytes = std::fs::read(dmg).with_context(|| format!("reading {dmg}"))?;
    let size = json["size"].as_u64().context("the feed has no size")?;
    if bytes.len() as u64 != size {
        bail!("the DMG is {} bytes, the feed says {size}", bytes.len());
    }
    let key = data_encoding::BASE64.decode(pubkey.trim().as_bytes()).context("the public key is not base64")?;
    let sig = data_encoding::BASE64.decode(json["ed25519"].as_str().context("the feed has no signature")?.trim().as_bytes()).context("the signature is not base64")?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &key).verify(&bytes, &sig).map_err(|_| anyhow!("signature mismatch"))?;
    println!("{version}: {size} bytes, signature valid");
    Ok(())
}
