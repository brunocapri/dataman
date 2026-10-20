use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

use ring::signature::{Ed25519KeyPair, KeyPair};
use dataman_core::update::{self, Feed, UpdateConfig};

fn app(dir: &Path, version: &str) -> PathBuf {
    let app = dir.join("Dataman.app");
    std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>dataman</string>
<key>CFBundleIdentifier</key><string>dev.dataman.test</string>
<key>CFBundleName</key><string>Dataman</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
<key>CFBundleVersion</key><string>{version}</string>
</dict></plist>"#
    );
    std::fs::write(app.join("Contents/Info.plist"), plist).unwrap();
    let exe = app.join("Contents/MacOS/dataman");
    std::fs::write(&exe, format!("#!/bin/sh\necho {version}\n")).unwrap();
    run(Command::new("chmod").arg("+x").arg(&exe));
    run(Command::new("codesign").args(["--force", "--sign", "-"]).arg(&app));
    app
}

fn run(c: &mut Command) {
    let out = c.output().unwrap();
    assert!(out.status.success(), "{c:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn dmg(app_dir: &Path, out: &Path) {
    run(Command::new("hdiutil").args(["create", "-quiet", "-ov", "-fs", "HFS+", "-format", "UDZO", "-volname", "Dataman", "-srcfolder"]).arg(app_dir).arg(out));
}

fn serve(root: PathBuf) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let root = root.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap() == 0 || h == "\r\n" {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("/").trim_start_matches('/').to_string();
                let mut s = s;
                match std::fs::read(root.join(&path)) {
                    Ok(body) => {
                        let _ = write!(s, "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                        let _ = s.write_all(&body);
                    }
                    Err(_) => {
                        let _ = s.write_all(b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                    }
                }
            });
        }
    });
    port
}

#[test]
fn check_download_verify_install() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let doc = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
    let kp = Ed25519KeyPair::from_pkcs8(doc.as_ref()).unwrap();
    let key = data_encoding::BASE64.encode(kp.public_key().as_ref());

    let apps = t.join("Applications");
    let installed = app(&apps, "0.1.0");
    let www = t.join("www");
    std::fs::create_dir_all(&www).unwrap();
    dmg(app(&t.join("v2"), "0.2.0").parent().unwrap(), &www.join("new.dmg"));
    dmg(app(&t.join("v15"), "0.1.5").parent().unwrap(), &www.join("old.dmg"));
    let bytes = std::fs::read(www.join("new.dmg")).unwrap();
    let mut tampered = bytes.clone();
    let mid = tampered.len() / 2;
    tampered[mid] ^= 0xff;
    std::fs::write(www.join("tampered.dmg"), &tampered).unwrap();
    let old = std::fs::read(www.join("old.dmg")).unwrap();

    let port = serve(www.clone());
    let base = format!("http://127.0.0.1:{port}");
    let feed_for = |name: &str, file: &str, size: usize, sig_of: &[u8], version: &str| {
        let f = Feed {
            version: version.into(),
            notes: "Faster grid.".into(),
            url: format!("{base}/{file}"),
            size: size as u64,
            ed25519: data_encoding::BASE64.encode(kp.sign(sig_of).as_ref()),
            minimum_macos: Some("11.0".into()),
            published: None,
        };
        std::fs::write(www.join(name), serde_json::to_vec(&f).unwrap()).unwrap();
        format!("{base}/{name}")
    };
    let good = feed_for("feed.json", "new.dmg", bytes.len(), &bytes, "0.2.0");
    let bad_sig = feed_for("tampered.json", "tampered.dmg", bytes.len(), &bytes, "0.2.0");
    let bad_size = feed_for("size.json", "new.dmg", bytes.len() + 10, &bytes, "0.2.0");
    let replay = feed_for("replay.json", "old.dmg", old.len(), &old, "0.2.0");
    let cache = t.join("cache");

    let cfg = |url: &str, current: &str| UpdateConfig::new(Some(url), Some(&key), current).unwrap();
    assert_eq!(update::check(&cfg(&good, "0.2.0")).unwrap(), None);
    assert!(update::check(&cfg(&format!("{base}/missing.json"), "0.1.0")).is_err());

    for (url, err) in [(&bad_sig, "signature"), (&bad_size, "size")] {
        let c = cfg(url, "0.1.0");
        let feed = update::check(&c).unwrap().expect("offered");
        let e = update::download(&c, &feed, &cache, &|_| {}).unwrap_err().to_string();
        assert!(e.contains(err), "{e}");
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);
    }
    let c = cfg(&replay, "0.1.0");
    let feed = update::check(&c).unwrap().unwrap();
    let file = update::download(&c, &feed, &cache, &|_| {}).unwrap();
    let e = update::install(&file, &installed, &feed.version).unwrap_err().to_string();
    assert!(e.contains("0.1.5"), "{e}");
    assert_eq!(update::plist_value(&installed, "CFBundleShortVersionString").unwrap(), "0.1.0");

    let c = cfg(&good, "0.1.0");
    let feed = update::check(&c).unwrap().expect("0.2.0 offered");
    assert_eq!((feed.version.as_str(), feed.notes.as_str()), ("0.2.0", "Faster grid."));
    let seen = std::sync::Mutex::new(Vec::new());
    let file = update::download(&c, &feed, &cache, &|p| seen.lock().unwrap().push(p)).unwrap();
    let seen = seen.into_inner().unwrap();
    assert!(matches!(seen.last(), Some(update::Phase::Verifying)), "{seen:?}");
    assert!(seen.contains(&update::Phase::Got(feed.size)), "{seen:?}");
    run(Command::new("chmod").arg("a-w").arg(&apps));
    let e = update::install(&file, &installed, &feed.version).unwrap_err().to_string();
    run(Command::new("chmod").arg("u+w").arg(&apps));
    assert!(e.contains("not writable"), "{e}");
    update::install(&file, &installed, &feed.version).unwrap();
    assert_eq!(update::plist_value(&installed, "CFBundleShortVersionString").unwrap(), "0.2.0");
    let out = Command::new(installed.join("Contents/MacOS/dataman")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "0.2.0");
    let names: Vec<String> = std::fs::read_dir(&apps).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, ["Dataman.app"]);
    assert_eq!(update::download(&c, &feed, &cache, &|_| {}).unwrap(), file);
}
