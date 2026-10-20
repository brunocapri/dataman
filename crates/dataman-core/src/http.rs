//! HTTP through reqwest on the rustls the database drivers use (ring): no curl, which not every
//! Linux desktop has. Blocking calls, for background threads.

use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};

use crate::update::Phase;

fn run<T>(f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    crate::backend::install_tls_provider();
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(f)
}

fn client(timeout: Duration) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().connect_timeout(Duration::from_secs(15)).timeout(timeout).user_agent(concat!("Dataman/", env!("CARGO_PKG_VERSION"))).build()?)
}

fn local(url: &str) -> Option<&str> {
    url.strip_prefix("file://")
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn get(url: &str, max: u64, what: &str) -> Result<Vec<u8>> {
    if let Some(path) = local(url) {
        let bytes = std::fs::read(path).with_context(|| format!("{what}: {path}"))?;
        if bytes.len() as u64 > max {
            bail!("{what}: larger than {max} bytes");
        }
        return Ok(bytes);
    }
    run(async {
        let mut r = client(Duration::from_secs(30))?.get(url).send().await.with_context(|| what.to_string())?.error_for_status().with_context(|| what.to_string())?;
        let mut out = Vec::new();
        while let Some(c) = r.chunk().await? {
            out.extend_from_slice(&c);
            if out.len() as u64 > max {
                bail!("{what}: larger than {max} bytes");
            }
        }
        Ok(out)
    })
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn download(url: &str, size: u64, part: &Path, on: &dyn Fn(Phase)) -> Result<()> {
    let mut f = std::fs::File::create(part)?;
    if let Some(path) = local(url) {
        let bytes = std::fs::read(path).with_context(|| format!("download: {path}"))?;
        if bytes.len() as u64 > size {
            bail!("download: larger than the feed says");
        }
        f.write_all(&bytes)?;
        on(Phase::Got(bytes.len() as u64));
        return Ok(());
    }
    run(async {
        let mut r = client(Duration::from_secs(1800))?.get(url).send().await.context("download")?.error_for_status().context("download")?;
        let mut got = 0u64;
        while let Some(c) = r.chunk().await.context("download")? {
            got += c.len() as u64;
            if got > size {
                bail!("download: larger than the feed says");
            }
            f.write_all(&c)?;
            on(Phase::Got(got));
        }
        Ok(())
    })
}

/// POSTs `text` as text/plain; a redirect is not followed (a crash report goes where it is sent).
pub fn post_text(url: &str, text: &str, what: &str) -> Result<()> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        bail!("{what}: not an http(s) URL");
    }
    let (url, text) = (url.to_string(), text.to_string());
    run(async move {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("Dataman/", env!("CARGO_PKG_VERSION")))
            .build()?
            .post(&url)
            .header("Content-Type", "text/plain; charset=utf-8")
            .body(text)
            .send()
            .await
            .with_context(|| what.to_string())?
            .error_for_status()
            .with_context(|| what.to_string())?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};

    #[test]
    fn posts_text_and_says_why_it_failed() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let mut got = Vec::new();
            for status in ["200 OK", "500 Internal Server Error"] {
                let (s, _) = listener.accept().unwrap();
                let mut r = BufReader::new(s.try_clone().unwrap());
                let (mut line, mut len) = (String::new(), 0usize);
                let mut head = String::new();
                while r.read_line(&mut line).unwrap() > 2 {
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    head.push_str(&line);
                    line.clear();
                }
                let mut body = vec![0; len];
                r.read_exact(&mut body).unwrap();
                got.push((head, String::from_utf8(body).unwrap()));
                write!(&s, "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
            got
        });
        let url = format!("http://127.0.0.1:{port}/crash");
        super::post_text(&url, "boom\nnaïve", "crash report").unwrap();
        let err = super::post_text(&url, "again", "crash report").unwrap_err();
        assert!(format!("{err:#}").contains("500"), "{err:#}");
        let got = server.join().unwrap();
        assert!(got[0].0.starts_with("POST /crash ") && got[0].0.contains("text/plain"), "{}", got[0].0);
        assert_eq!(got[0].1, "boom\nnaïve");
        assert!(super::post_text("file:///etc/passwd", "x", "crash report").is_err());
    }
}
