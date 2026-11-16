//! SQL Server's login as Azure SQL answers it while a paused serverless database resumes: the
//! prelogin and TLS go through, and the login is refused with 40613.
#![allow(dead_code)]

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, OnceLock};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// Microsoft's text for 40613, as a login to a paused database gets it.
pub const PAUSED: &str = "Database 'dataman_t_wake' on server 'fake' is not currently available. Please retry the connection later. If the problem persists, contact customer support, and provide them the session tracing ID of '00000000-0000-0000-0000-000000000000'.";

fn tls() -> Arc<rustls::ServerConfig> {
    static C: OnceLock<Arc<rustls::ServerConfig>> = OnceLock::new();
    C.get_or_init(|| {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()]).unwrap().self_signed(&key).unwrap();
        let mut c = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![CertificateDer::from(cert.der().to_vec())], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())))
            .unwrap();
        // Nothing after the handshake but the login's answer.
        c.send_tls13_tickets = 0;
        Arc::new(c)
    })
    .clone()
}

fn read_packet(s: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut h = [0u8; 8];
    s.read_exact(&mut h)?;
    let len = u16::from_be_bytes([h[2], h[3]]) as usize;
    let mut body = vec![0; len.saturating_sub(8)];
    s.read_exact(&mut body)?;
    Ok((h[0], body))
}

fn packet(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut p = vec![kind, 0x01];
    p.extend(((body.len() + 8) as u16).to_be_bytes());
    p.extend([0, 0, 1, 0]);
    p.extend(body);
    p
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// An ERROR token, then a DONE that marks the error (TDS 7.4: a 4-byte line, an 8-byte count).
fn refusal(code: u32, message: &str) -> Vec<u8> {
    let mut e = Vec::new();
    e.extend(code.to_le_bytes());
    e.extend([1, 17]);
    e.extend((message.encode_utf16().count() as u16).to_le_bytes());
    e.extend(utf16(message));
    e.push(4);
    e.extend(utf16("fake"));
    e.push(0);
    e.extend(1u32.to_le_bytes());
    let mut t = vec![0xAA];
    t.extend((e.len() as u16).to_le_bytes());
    t.extend(e);
    t.push(0xFD);
    t.extend(2u16.to_le_bytes());
    t.extend(0u16.to_le_bytes());
    t.extend(0u64.to_le_bytes());
    t
}

/// Serves one connection: the login is refused with `code`.
pub fn refuse_login(mut s: TcpStream, code: u32, message: &str) -> io::Result<()> {
    let (kind, _) = read_packet(&mut s)?;
    assert_eq!(kind, 0x12, "a prelogin first");
    // VERSION (16.0.2000) and ENCRYPTION = ON: what Dataman's Require asks for.
    let pre = [0x00, 0x00, 0x0B, 0x00, 0x06, 0x01, 0x00, 0x11, 0x00, 0x01, 0xFF, 0x10, 0x00, 0x07, 0xD0, 0x00, 0x00, 0x01];
    s.write_all(&packet(0x04, &pre))?;
    let mut tls = rustls::ServerConnection::new(tls()).unwrap();
    // The handshake travels inside prelogin packets.
    while tls.is_handshaking() {
        if tls.wants_write() {
            let mut out = Vec::new();
            tls.write_tls(&mut out)?;
            s.write_all(&packet(0x12, &out))?;
            continue;
        }
        let (_, body) = read_packet(&mut s)?;
        let mut rd = &body[..];
        while !rd.is_empty() {
            tls.read_tls(&mut rd)?;
            tls.process_new_packets().map_err(io::Error::other)?;
        }
    }
    while tls.wants_write() {
        tls.write_tls(&mut s)?;
    }
    // Then plain TLS records: wait for the whole login.
    let mut got = Vec::new();
    loop {
        if got.len() >= 8 && got.len() >= u16::from_be_bytes([got[2], got[3]]) as usize {
            break;
        }
        if tls.read_tls(&mut s)? == 0 {
            return Ok(());
        }
        tls.process_new_packets().map_err(io::Error::other)?;
        let mut buf = [0u8; 8192];
        while let Ok(n) = tls.reader().read(&mut buf) {
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
    }
    assert_eq!(got[0], 0x10, "a LOGIN7");
    tls.writer().write_all(&packet(0x04, &refusal(code, message)))?;
    tls.send_close_notify();
    while tls.wants_write() {
        tls.write_tls(&mut s)?;
    }
    s.shutdown(std::net::Shutdown::Both)
}

/// A `gate::Door::Answer` for a paused Azure SQL serverless database.
pub fn paused(s: TcpStream) {
    if let Err(e) = refuse_login(s, 40613, PAUSED) {
        eprintln!("fake SQL Server: {e}");
    }
}
