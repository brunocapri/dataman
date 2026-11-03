//! Durations as the UI writes them, in one place.

use std::time::Duration;

/// A run's time in lists (history, agents): `340 ms`, `2.4 s`.
pub fn ms(ms: f64) -> String {
    if ms < 1000. { format!("{ms:.0} ms") } else { format!("{:.1} s", ms / 1000.) }
}

/// A round trip (the connection test): `0.8 ms`, `42 ms`.
pub fn rtt(ms: f64) -> String {
    if ms < 10. { format!("{ms:.1} ms") } else { format!("{ms:.0} ms") }
}

/// A statement's time in the result bar: `80 µs` to `3.25 s`.
pub fn dur(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1e3;
    if ms < 1. {
        format!("{:.0} µs", ms * 1e3)
    } else if ms < 100. {
        format!("{ms:.1} ms")
    } else if ms < 1000. {
        format!("{ms:.0} ms")
    } else {
        format!("{:.2} s", ms / 1e3)
    }
}

/// A long job (export, import): `12.3 s`, `4m 05s`.
pub fn secs(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 60. { format!("{s:.1} s") } else { format!("{}m {:02}s", (s / 60.) as u64, (s % 60.) as u64) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_written() {
        assert_eq!((ms(340.), ms(2400.)), ("340 ms".into(), "2.4 s".into()));
        assert_eq!((rtt(0.84), rtt(42.2)), ("0.8 ms".into(), "42 ms".into()));
        assert_eq!(dur(Duration::from_micros(80)), "80 µs");
        assert_eq!(dur(Duration::from_millis(3250)), "3.25 s");
        assert_eq!((secs(Duration::from_millis(12_300)), secs(Duration::from_secs(245))), ("12.3 s".into(), "4m 05s".into()));
    }
}
