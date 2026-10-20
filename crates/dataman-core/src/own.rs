use std::collections::HashMap;
use std::sync::LazyLock;

use parking_lot::Mutex;

use crate::config::ConnectionProfile;

static OWN: LazyLock<Mutex<HashMap<String, HashMap<i32, usize>>>> = LazyLock::new(Default::default);

pub fn server_key(p: &ConnectionProfile) -> String {
    let host = p.host.trim().to_ascii_lowercase();
    let host = match host.as_str() {
        "" | "localhost" | "127.0.0.1" | "::1" | "[::1]" => "local",
        h if h.starts_with('/') => "local",
        h => h,
    };
    format!("{host}:{}", p.port)
}

#[derive(Debug)]
pub struct Mark {
    key: String,
    pid: i32,
}

impl Mark {
    pub fn pid(&self) -> i32 {
        self.pid
    }
}

pub fn mark(key: String, pid: i32) -> Mark {
    *OWN.lock().entry(key.clone()).or_default().entry(pid).or_default() += 1;
    Mark { key, pid }
}

impl Drop for Mark {
    fn drop(&mut self) {
        let mut g = OWN.lock();
        if let Some(m) = g.get_mut(&self.key) {
            if let Some(n) = m.get_mut(&self.pid) {
                *n -= 1;
                if *n == 0 {
                    m.remove(&self.pid);
                }
            }
            if m.is_empty() {
                g.remove(&self.key);
            }
        }
    }
}

pub fn is_own(key: &str, pid: i32) -> bool {
    OWN.lock().get(key).is_some_and(|m| m.contains_key(&pid))
}

#[cfg(test)]
pub fn pids(key: &str) -> Vec<i32> {
    let mut v: Vec<i32> = OWN.lock().get(key).map(|m| m.keys().copied().collect()).unwrap_or_default();
    v.sort_unstable();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_come_and_go() {
        let mut p = ConnectionProfile::new("x");
        (p.host, p.port) = ("LocalHost".into(), 6543);
        let key = server_key(&p);
        assert_eq!(key, "local:6543");
        p.host = "127.0.0.1".into();
        assert_eq!(server_key(&p), key);
        p.host = "/tmp".into();
        assert_eq!(server_key(&p), key);
        p.host = "db.internal".into();
        assert_eq!(server_key(&p), "db.internal:6543");

        assert!(!is_own(&key, 41));
        let a = mark(key.clone(), 41);
        let b = mark(key.clone(), 41);
        let c = mark(key.clone(), 42);
        assert!(is_own(&key, 41) && is_own(&key, 42));
        assert!(!is_own("db.internal:6543", 41));
        assert_eq!(pids(&key), vec![41, 42]);
        drop(a);
        assert!(is_own(&key, 41), "still held by the other mark");
        drop(b);
        assert!(!is_own(&key, 41));
        assert_eq!(c.pid(), 42);
        drop(c);
        assert!(pids(&key).is_empty());
    }
}
