//! The transaction status of an editor session, read off its socket.
//!
//! Every ReadyForQuery message carries the backend's transaction status: `I` idle, `T` inside a
//! transaction block, `E` inside a failed one. tokio-postgres reads it and drops it, so the
//! session's socket goes through [`Sniff`], which watches the backend's message frames go by
//! (only the 5-byte headers are looked at; no copy) and hands each status to the session's
//! [`TxWatch`]. Over TLS the sniffer sits above the TLS stream, where the messages are plain.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::tx::{Seen, TxWatch};

/// The SSLRequest a client sends before the startup message when it may use TLS.
const SSL_REQUEST: [u8; 8] = [0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f];

/// Follows the backend's message frames (a type byte, a 4-byte big-endian length that counts
/// itself, the body) and reports each ErrorResponse (`E`), the status byte of each
/// ReadyForQuery (`Z`) and the process id of the BackendKeyData (`K`).
#[derive(Debug, Default)]
pub(crate) struct Frames {
    head: [u8; 5],
    /// Header bytes read so far; 5 while in a body.
    have: usize,
    /// Body bytes left.
    left: usize,
    /// Body bytes read so far.
    pos: usize,
    /// The first bytes of a BackendKeyData body (the process id).
    key: [u8; 4],
    /// The client's first write was an SSLRequest: the next byte read is the server's one-byte
    /// answer, outside any frame.
    ssl_answer: bool,
    /// The server said yes to TLS: what follows here is ciphertext (the TLS layer has its own
    /// sniffer).
    off: bool,
    wrote: bool,
}

impl Frames {
    pub(crate) fn on_write(&mut self, buf: &[u8]) {
        if !self.wrote && !buf.is_empty() {
            self.wrote = true;
            self.ssl_answer = buf.starts_with(&SSL_REQUEST);
        }
    }

    pub(crate) fn feed(&mut self, mut data: &[u8], mut seen: impl FnMut(Seen)) {
        while !data.is_empty() && !self.off {
            if self.ssl_answer {
                self.ssl_answer = false;
                if data[0] == b'S' {
                    self.off = true;
                    return;
                }
                data = &data[1..];
                continue;
            }
            if self.have < 5 {
                let n = (5 - self.have).min(data.len());
                self.head[self.have..self.have + n].copy_from_slice(&data[..n]);
                self.have += n;
                data = &data[n..];
                if self.have == 5 {
                    if self.head[0] == b'E' {
                        seen(Seen::Error);
                    }
                    let len = u32::from_be_bytes([self.head[1], self.head[2], self.head[3], self.head[4]]) as usize;
                    self.left = len.saturating_sub(4);
                    self.pos = 0;
                    if self.left == 0 {
                        self.have = 0;
                    }
                }
                continue;
            }
            if self.pos == 0 && self.head[0] == b'Z' {
                seen(Seen::Ready(data[0]));
            }
            let n = self.left.min(data.len());
            if self.head[0] == b'K' && self.pos < 4 {
                let m = (4 - self.pos).min(n);
                self.key[self.pos..self.pos + m].copy_from_slice(&data[..m]);
                if self.pos + m == 4 {
                    seen(Seen::Pid(i32::from_be_bytes(self.key)));
                }
            }
            self.pos += n;
            self.left -= n;
            data = &data[n..];
            if self.left == 0 {
                self.have = 0;
            }
        }
    }
}

/// A stream whose incoming backend messages are watched for ReadyForQuery.
pub(crate) struct Sniff<S> {
    inner: S,
    frames: Frames,
    watch: Arc<TxWatch>,
}

impl<S> Sniff<S> {
    pub(crate) fn new(inner: S, watch: Arc<TxWatch>) -> Sniff<S> {
        Sniff { inner, frames: Frames::default(), watch }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Sniff<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let r = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = r {
            let watch = &this.watch;
            this.frames.feed(&buf.filled()[before..], |f| watch.seen(f));
        }
        r
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Sniff<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.frames.on_write(buf);
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

impl<S: tokio_postgres::tls::TlsStream + Unpin> tokio_postgres::tls::TlsStream for Sniff<S> {
    fn channel_binding(&self) -> tokio_postgres::tls::ChannelBinding {
        self.inner.channel_binding()
    }
}

/// A TLS connector whose stream is sniffed (the messages are plain above TLS), and which
/// records whether the handshake happened.
pub(crate) struct SniffTls<T> {
    pub inner: T,
    pub watch: Arc<TxWatch>,
    pub used: Arc<AtomicBool>,
}

impl<S, T> tokio_postgres::tls::TlsConnect<S> for SniffTls<T>
where
    T: tokio_postgres::tls::TlsConnect<S>,
    T::Future: Send + 'static,
    T::Stream: Unpin,
{
    type Stream = Sniff<T::Stream>;
    type Error = T::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Sniff<T::Stream>, T::Error>> + Send>>;

    fn connect(self, stream: S) -> Self::Future {
        let fut = self.inner.connect(stream);
        let (watch, used) = (self.watch, self.used);
        Box::pin(async move {
            let s = fut.await?;
            used.store(true, Ordering::Release);
            Ok(Sniff::new(s, watch))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::tx::TxStatus;

    fn msg(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![kind];
        v.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
        v.extend_from_slice(body);
        v
    }

    fn statuses(f: &mut Frames, chunks: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for c in chunks {
            f.feed(c, |s| {
                if let Seen::Ready(b) = s {
                    out.push(b)
                }
            });
        }
        out
    }

    /// An ErrorResponse is reported as soon as its header is in, before the ReadyForQuery that
    /// follows it; a waiter is released by that ReadyForQuery (or the connection closing).
    #[test]
    fn error_then_ready() {
        let mut stream = msg(b'E', b"SERROR\0C57014\0\0");
        let at = stream.len();
        stream.extend(msg(b'Z', b"E"));
        let mut f = Frames::default();
        let mut seen = Vec::new();
        f.feed(&stream[..5], |s| seen.push(s));
        assert_eq!(seen, [Seen::Error], "the header is enough");
        f.feed(&stream[5..at], |s| seen.push(s));
        f.feed(&stream[at..], |s| seen.push(s));
        assert_eq!(seen, [Seen::Error, Seen::Ready(b'E')]);

        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let w = Arc::new(TxWatch::default());
        w.seen(Seen::Ready(b'T'));
        w.seen(Seen::Error);
        let t0 = Instant::now();
        rt.block_on(w.settled(std::time::Duration::from_millis(50)));
        assert!(t0.elapsed() >= std::time::Duration::from_millis(50), "waits while the error's ReadyForQuery is out");
        let w2 = w.clone();
        rt.block_on(async move {
            let h = tokio::spawn(async move { w2.settled(std::time::Duration::from_secs(5)).await });
            tokio::task::yield_now().await;
            w.seen(Seen::Ready(b'E'));
            tokio::time::timeout(std::time::Duration::from_secs(1), h).await.expect("released").unwrap();
            assert_eq!(w.get().status, TxStatus::Failed);
            w.seen(Seen::Error);
            w.closed();
            tokio::time::timeout(std::time::Duration::from_millis(100), w.settled(std::time::Duration::from_secs(5))).await.expect("closed");
        });
    }

    #[test]
    fn ready_for_query_in_any_split() {
        let mut stream = Vec::new();
        stream.extend(msg(b'T', &[0, 1, b'x', 0]));
        stream.extend(msg(b'D', &[0, 1, 0, 0, 0, 1, b'Z']));
        stream.extend(msg(b'C', b"SELECT 1\0"));
        stream.extend(msg(b'Z', b"T"));
        stream.extend(msg(b'E', b"SERROR\0"));
        stream.extend(msg(b'Z', b"E"));
        stream.extend(msg(b'1', b""));
        stream.extend(msg(b'Z', b"I"));
        // Whole, then every split point, then one byte at a time: the same statuses.
        assert_eq!(statuses(&mut Frames::default(), &[&stream]), b"TEI");
        for cut in 0..stream.len() {
            let (a, b) = stream.split_at(cut);
            assert_eq!(statuses(&mut Frames::default(), &[a, b]), b"TEI", "cut at {cut}");
        }
        let bytes: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(statuses(&mut Frames::default(), &bytes), b"TEI");
    }

    /// The backend's process id comes from BackendKeyData, whatever the reads look like.
    #[test]
    fn backend_pid_in_any_split() {
        let mut stream = Vec::new();
        stream.extend(msg(b'R', &[0, 0, 0, 0]));
        stream.extend(msg(b'S', b"server_version\x0018\0"));
        stream.extend(msg(b'K', &[0, 0, 0x30, 0x39, 0xde, 0xad, 0xbe, 0xef]));
        stream.extend(msg(b'Z', b"I"));
        let pids = |chunks: &[&[u8]]| {
            let mut f = Frames::default();
            let mut out = Vec::new();
            for c in chunks {
                f.feed(c, |s| match s {
                    Seen::Pid(p) => out.push(p),
                    Seen::Ready(b) => out.push(-(b as i32)),
                    Seen::Error => {}
                });
            }
            out
        };
        let want = vec![12345, -(b'I' as i32)];
        assert_eq!(pids(&[&stream]), want);
        for cut in 0..stream.len() {
            let (a, b) = stream.split_at(cut);
            assert_eq!(pids(&[a, b]), want, "cut at {cut}");
        }
        let bytes: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(pids(&bytes), want);
        let w = TxWatch::default();
        assert_eq!(w.pid(), None);
        w.seen(Seen::Pid(12345));
        assert_eq!(w.pid(), Some(12345));
    }

    #[test]
    fn ssl_negotiation() {
        // The server declines TLS: its `N` is outside any frame, then plain messages follow.
        let mut f = Frames::default();
        f.on_write(&SSL_REQUEST);
        let mut s = vec![b'N'];
        s.extend(msg(b'Z', b"I"));
        assert_eq!(statuses(&mut f, &[&s]), b"I");
        // It accepts: the rest is ciphertext, ignored here.
        let mut f = Frames::default();
        f.on_write(&SSL_REQUEST);
        assert_eq!(statuses(&mut f, &[b"S", &msg(b'Z', b"T")]), b"");
        // No SSLRequest (sslmode=disable): framed from the first byte.
        let mut f = Frames::default();
        f.on_write(&[0, 0, 0, 9, 0, 3, 0, 0, 0]);
        assert_eq!(statuses(&mut f, &[&msg(b'Z', b"I")]), b"I");
    }
}
