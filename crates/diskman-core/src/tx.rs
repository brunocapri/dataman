//! An editor session's transaction state, in the server's own words.
//!
//! The driver reports what the server says about the session's transaction to its [`TxWatch`]
//! ([`Seen`]): PostgreSQL's ReadyForQuery messages carry the status byte (`I` idle, `T` inside a
//! transaction block, `E` inside a failed one), read off the socket by `backend::pg::sniff`.
//!
//! [`TxState`] is the small state machine the UI reads: the status, when the transaction began
//! and how many of the user's statements ran in it.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Instant;

use parking_lot::Mutex;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TxStatus {
    #[default]
    Idle,
    /// Inside a transaction block.
    Open,
    /// Inside a failed transaction block: the server refuses everything until it ends.
    Failed,
}

impl TxStatus {
    /// The status byte of a ReadyForQuery message.
    pub fn from_byte(b: u8) -> Option<TxStatus> {
        match b {
            b'I' => Some(TxStatus::Idle),
            b'T' => Some(TxStatus::Open),
            b'E' => Some(TxStatus::Failed),
            _ => None,
        }
    }

    pub fn is_open(self) -> bool {
        self != TxStatus::Idle
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxState {
    pub status: TxStatus,
    /// When the transaction began (the first ReadyForQuery inside it).
    pub since: Option<Instant>,
    /// The user's statements that ran in it (transaction control does not count).
    pub statements: u32,
}

impl TxState {
    /// A ReadyForQuery arrived.
    pub fn ready(&mut self, status: TxStatus, now: Instant) {
        match (self.status.is_open(), status.is_open()) {
            (false, true) => {
                self.since = Some(now);
                self.statements = 0;
            }
            (true, false) => {
                self.since = None;
                self.statements = 0;
            }
            _ => {}
        }
        self.status = status;
    }

    /// One of the user's statements finished (`counts`: not transaction control).
    pub fn statement(&mut self, counts: bool) {
        if counts && self.status.is_open() {
            self.statements += 1;
        }
    }

    /// The connection is gone, and its transaction with it (the server rolled it back).
    pub fn closed(&mut self) {
        *self = TxState::default();
    }
}

/// The transaction state shared by a session and the driver reading its connection.
///
/// tokio-postgres hands an ErrorResponse to the statement as soon as it arrives, and the
/// ReadyForQuery that follows (the status after the error) may come in a later read: the server
/// flushes the error at once, and a tunnel or a slow link splits them. `error_pending` marks
/// that gap, and [`TxWatch::settled`] waits it out before a failed statement is reported.
#[derive(Debug, Default)]
pub struct TxWatch {
    state: Mutex<TxState>,
    error_pending: AtomicBool,
    settle: tokio::sync::Notify,
    /// The backend's process id, from its BackendKeyData (0 until it arrives).
    pid: AtomicI32,
}

impl TxWatch {
    pub fn get(&self) -> TxState {
        *self.state.lock()
    }

    /// The backend's process id (`pg_backend_pid()`), once the server sent it.
    pub fn pid(&self) -> Option<i32> {
        Some(self.pid.load(Ordering::Acquire)).filter(|p| *p != 0)
    }

    pub(crate) fn seen(&self, frame: Seen) {
        match frame {
            Seen::Pid(p) => self.pid.store(p, Ordering::Release),
            Seen::Error => self.error_pending.store(true, Ordering::Release),
            Seen::Ready(b) => {
                if let Some(s) = TxStatus::from_byte(b) {
                    self.state.lock().ready(s, Instant::now());
                }
                self.error_pending.store(false, Ordering::Release);
                self.settle.notify_waiters();
            }
        }
    }

    /// Returns once the ReadyForQuery after the last ErrorResponse has been read (at once when
    /// none is outstanding), or after `limit`.
    pub(crate) async fn settled(&self, limit: std::time::Duration) {
        let _ = tokio::time::timeout(limit, async {
            loop {
                let notified = self.settle.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if !self.error_pending.load(Ordering::Acquire) {
                    return;
                }
                notified.await;
            }
        })
        .await;
    }

    pub(crate) fn statement(&self, counts: bool) {
        self.state.lock().statement(counts);
    }

    pub(crate) fn closed(&self) {
        self.state.lock().closed();
        self.error_pending.store(false, Ordering::Release);
        self.settle.notify_waiters();
    }
}

/// What a driver saw on a session's connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Seen {
    /// An error response began (the status after it is still to come).
    Error,
    /// The server is ready for the next statement, with its transaction status byte
    /// ([`TxStatus::from_byte`]).
    Ready(u8),
    /// The backend's process id (PostgreSQL's BackendKeyData).
    Pid(i32),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_machine() {
        let t0 = Instant::now();
        let later = t0 + std::time::Duration::from_secs(5);
        let mut s = TxState::default();
        s.ready(TxStatus::Idle, t0);
        s.statement(true);
        assert_eq!(s, TxState::default(), "outside a transaction nothing is counted");
        // BEGIN opens it: the time starts, BEGIN itself is not counted.
        s.ready(TxStatus::Open, t0);
        s.statement(false);
        assert_eq!((s.status, s.since, s.statements), (TxStatus::Open, Some(t0), 0));
        s.ready(TxStatus::Open, later);
        s.statement(true);
        s.ready(TxStatus::Open, later);
        s.statement(true);
        assert_eq!((s.since, s.statements), (Some(t0), 2), "the start stays");
        // An error: failed, still open, same start.
        s.ready(TxStatus::Failed, later);
        s.statement(true);
        assert_eq!((s.status, s.since, s.statements), (TxStatus::Failed, Some(t0), 3));
        assert!(s.status.is_open());
        // ROLLBACK TO SAVEPOINT recovers it.
        s.ready(TxStatus::Open, later);
        assert_eq!((s.status, s.statements), (TxStatus::Open, 3));
        // COMMIT ends it.
        s.ready(TxStatus::Idle, later);
        assert_eq!(s, TxState { status: TxStatus::Idle, since: None, statements: 0 });
        // A dropped connection loses it.
        s.ready(TxStatus::Open, later);
        s.closed();
        assert_eq!(s, TxState::default());
        assert_eq!(TxStatus::from_byte(b'?'), None);
    }
}
