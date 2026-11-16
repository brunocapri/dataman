//! What Dataman knows of a connection's way to its server, and how a dial gets through a database
//! that is waking up. Nothing here asks the server anything on its own: a background query or
//! login would keep waking a serverless database. The state changes only by what some use of the
//! connection learned.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use parking_lot::Mutex;
use tokio::time::Instant;

/// A socket idle this long is asked whether it still answers before a statement goes out on it.
/// Statements run back to back (a script of a thousand) pay nothing.
pub const CHECK_AFTER: Duration = Duration::from_secs(2);
/// A socket that takes longer than this to answer the check counts as gone: a laptop that slept
/// or a network that changed sends no reset, and the statement would hang for minutes instead.
pub const CHECK_WAIT: Duration = Duration::from_secs(10);

/// Pause between logins while a database wakes (Microsoft asks for at least 5 s).
pub const WAKE_PAUSE: Duration = Duration::from_secs(5);
/// How long after the first "waking" answer the logins go on. Microsoft: resume is usually under a
/// minute; measured on Azure SQL serverless: connected 62 s after the first login.
pub const WAKE_BOUND: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Link {
    #[default]
    Up,
    /// The server said the database is waking up, and the login is being retried.
    Waking,
    /// A socket of this connection died, and none has been made or heard from since. The next
    /// use reconnects.
    Lost,
}

#[derive(Debug, Default)]
pub struct LinkState {
    state: Mutex<Link>,
    changes: AtomicU64,
    stopped: AtomicBool,
}

impl LinkState {
    pub fn get(&self) -> Link {
        *self.state.lock()
    }

    /// Goes up on every change: what draws the state asks whether it should look again.
    pub fn changes(&self) -> u64 {
        self.changes.load(Ordering::Acquire)
    }

    /// Disconnected or forgotten: no more logins for a database still waking.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    fn set(&self, to: Link) {
        let mut g = self.state.lock();
        if *g != to {
            *g = to;
            self.changes.fetch_add(1, Ordering::AcqRel);
        }
    }

    pub(crate) fn up(&self) {
        self.set(Link::Up);
    }

    /// A login that is retrying says how it ends: one socket dying meanwhile changes nothing.
    pub(crate) fn lost(&self) {
        let mut g = self.state.lock();
        if *g == Link::Up {
            *g = Link::Lost;
            self.changes.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// A login that the server answers with "waking up" (`backend::class`: `ErrorClass::Waking`) ran
/// nothing on the server, so it is safe to try again. Any other failure ends it.
pub(crate) async fn through_wake<T, Fut>(link: &LinkState, attempt: impl FnMut() -> Fut) -> Result<T>
where
    Fut: Future<Output = Result<T>>,
{
    retry_while(link, crate::backend::is_waking, attempt).await
}

async fn retry_while<T, Fut>(link: &LinkState, waking: impl Fn(&anyhow::Error) -> bool, mut attempt: impl FnMut() -> Fut) -> Result<T>
where
    Fut: Future<Output = Result<T>>,
{
    let mut last = match attempt().await {
        Ok(v) => {
            link.up();
            return Ok(v);
        }
        Err(e) if waking(&e) => e,
        Err(e) => {
            // No answer from the server at all; one that refused the login is still there.
            if crate::backend::server_error(&e).is_none() {
                link.lost();
            }
            return Err(e);
        }
    };
    link.set(Link::Waking);
    let deadline = Instant::now() + WAKE_BOUND;
    while Instant::now() + WAKE_PAUSE < deadline && !link.stopped.load(Ordering::Acquire) {
        tokio::time::sleep(WAKE_PAUSE).await;
        match tokio::time::timeout_at(deadline, attempt()).await {
            Ok(Ok(v)) => {
                link.up();
                return Ok(v);
            }
            Ok(Err(e)) if waking(&e) => last = e,
            Ok(Err(e)) => {
                link.set(Link::Lost);
                return Err(e);
            }
            Err(_) => break,
        }
    }
    link.set(Link::Lost);
    Err(last.context(format!("the database did not wake up within {} s", WAKE_BOUND.as_secs())))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::backend::class;
    use crate::config::Engine;
    use crate::result::ErrorClass;

    #[derive(Debug, thiserror::Error)]
    #[error("{0}")]
    struct Fake(&'static str);

    fn is_wake(e: &anyhow::Error) -> bool {
        e.downcast_ref::<Fake>().is_some_and(|f| f.0 == "waking")
    }

    /// Fails `waking` times with "waking", then with `then`, each attempt taking `takes`.
    async fn fake(link: &LinkState, waking: usize, takes: Duration, then: Result<u8, &'static str>) -> (Result<u8>, usize, Duration) {
        let tried = AtomicUsize::new(0);
        let start = Instant::now();
        let r = retry_while(link, is_wake, || {
            let n = tried.fetch_add(1, Ordering::AcqRel);
            async move {
                tokio::time::sleep(takes).await;
                if n < waking {
                    return Err(Fake("waking").into());
                }
                then.map_err(|m| Fake(m).into())
            }
        })
        .await;
        (r, tried.load(Ordering::Acquire), start.elapsed())
    }

    #[test]
    fn what_means_waking_is_the_engines_own() {
        assert_eq!(class::of(Engine::SqlServer, "40613"), ErrorClass::Waking);
        for (engine, code) in [(Engine::SqlServer, "18456"), (Engine::SqlServer, "4060"), (Engine::SqlServer, "10054"), (Engine::MySql, "40613"), (Engine::Postgres, "40613"), (Engine::MySql, "1045"), (Engine::Postgres, "57P03")] {
            assert_ne!(class::of(engine, code), ErrorClass::Waking, "{engine:?} {code}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_waking_database_is_retried_until_it_answers() {
        let link = LinkState::default();
        let (r, tried, took) = fake(&link, 3, Duration::from_secs(15), Ok(7)).await;
        assert_eq!(r.unwrap(), 7);
        assert_eq!(tried, 4);
        assert_eq!(took, Duration::from_secs(15 * 4 + 5 * 3));
        assert_eq!(link.get(), Link::Up);
    }

    #[tokio::test(start_paused = true)]
    async fn the_retries_stop_at_the_bound_with_a_real_error() {
        let link = LinkState::default();
        let (r, tried, took) = fake(&link, usize::MAX, Duration::ZERO, Ok(0)).await;
        let msg = format!("{:#}", r.unwrap_err());
        assert!(msg.starts_with("the database did not wake up within 120 s") && msg.ends_with("waking"), "{msg}");
        // The first at 0, then one every 5 s while another pause still fits.
        assert_eq!((tried, took), (24, Duration::from_secs(115)));
        assert_eq!(link.get(), Link::Lost);
    }

    #[tokio::test(start_paused = true)]
    async fn a_login_that_hangs_is_cut_at_the_bound() {
        let link = LinkState::default();
        let (r, tried, took) = fake(&link, usize::MAX, Duration::from_secs(50), Ok(0)).await;
        assert!(r.is_err());
        assert_eq!(took, Duration::from_secs(50) + WAKE_BOUND);
        assert_eq!(tried, 4, "at 0, then started at 55, 110 and 165");
    }

    #[tokio::test(start_paused = true)]
    async fn any_other_failure_is_not_retried() {
        let link = LinkState::default();
        let (r, tried, _) = fake(&link, 0, Duration::ZERO, Err("login failed")).await;
        assert_eq!(r.unwrap_err().to_string(), "login failed");
        assert_eq!(tried, 1);
        let (r, tried, _) = fake(&link, 2, Duration::ZERO, Err("login failed")).await;
        assert_eq!(r.unwrap_err().to_string(), "login failed", "not hidden behind the wake");
        assert_eq!(tried, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_disconnect_stops_the_logins() {
        let link = LinkState::default();
        link.stop();
        let (r, tried, took) = fake(&link, usize::MAX, Duration::ZERO, Ok(0)).await;
        assert!(r.is_err());
        assert_eq!((tried, took), (1, Duration::ZERO));
    }

    #[test]
    fn a_socket_dying_does_not_end_a_wake() {
        let link = LinkState::default();
        assert_eq!(link.get(), Link::Up);
        let at = link.changes();
        link.lost();
        assert_eq!((link.get(), link.changes()), (Link::Lost, at + 1));
        link.lost();
        assert_eq!(link.changes(), at + 1, "no change, no redraw");
        link.up();
        link.set(Link::Waking);
        link.lost();
        assert_eq!(link.get(), Link::Waking);
    }
}
