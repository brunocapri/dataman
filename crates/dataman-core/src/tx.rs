use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Instant;

use parking_lot::Mutex;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TxStatus {
    #[default]
    Idle,
    Open,
    Failed,
}

impl TxStatus {
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
    pub since: Option<Instant>,
    pub statements: u32,
}

impl TxState {
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

    pub fn statement(&mut self, counts: bool) {
        if counts && self.status.is_open() {
            self.statements += 1;
        }
    }

    pub fn closed(&mut self) {
        *self = TxState::default();
    }
}

/// tokio-postgres hands an ErrorResponse to the statement as soon as it arrives, and the
/// ReadyForQuery that follows (the status after the error) may come in a later read: the server
/// flushes the error at once, and a tunnel or a slow link splits them. `error_pending` marks
/// that gap, and [`TxWatch::settled`] waits it out before a failed statement is reported.
#[derive(Debug, Default)]
pub struct TxWatch {
    state: Mutex<TxState>,
    error_pending: AtomicBool,
    settle: tokio::sync::Notify,
    pid: AtomicI32,
    notices: Mutex<Vec<String>>,
}

impl TxWatch {
    /// Capped: a loop of `RAISE NOTICE` must not fill memory.
    pub(crate) fn push_notice(&self, m: String) {
        let mut g = self.notices.lock();
        if g.len() < 1000 {
            g.push(m);
        }
    }

    pub(crate) fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut *self.notices.lock())
    }

    pub fn get(&self) -> TxState {
        *self.state.lock()
    }

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Seen {
    Error,
    Ready(u8),
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
        s.ready(TxStatus::Open, t0);
        s.statement(false);
        assert_eq!((s.status, s.since, s.statements), (TxStatus::Open, Some(t0), 0));
        s.ready(TxStatus::Open, later);
        s.statement(true);
        s.ready(TxStatus::Open, later);
        s.statement(true);
        assert_eq!((s.since, s.statements), (Some(t0), 2), "the start stays");
        s.ready(TxStatus::Failed, later);
        s.statement(true);
        assert_eq!((s.status, s.since, s.statements), (TxStatus::Failed, Some(t0), 3));
        assert!(s.status.is_open());
        s.ready(TxStatus::Open, later);
        assert_eq!((s.status, s.statements), (TxStatus::Open, 3));
        s.ready(TxStatus::Idle, later);
        assert_eq!(s, TxState { status: TxStatus::Idle, since: None, statements: 0 });
        s.ready(TxStatus::Open, later);
        s.closed();
        assert_eq!(s, TxState::default());
        assert_eq!(TxStatus::from_byte(b'?'), None);
    }
}
