mod common;
use common::*;

#[test]
fn lar_is_detected_and_postgres_is_not() {
    let e = env();
    if let Some(c) = lar(&e) {
        assert!(c.info.is_lar && c.lar().is_some(), "{}", c.info.version);
    }
    if let Some(c) = pg(&e) {
        assert!(!c.info.is_lar && c.lar().is_none(), "{}", c.info.version);
    }
}

fn wait_until(secs: u64, what: &str, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(secs), "timeout waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

mod live {
    use super::*;
    use dataman_core::lar::live::{ChangeKind, SubState, SubscribeOptions};

    #[test]
    fn listen_as_delivers_whole_results() {
        let e = env();
        let Some(c) = lar(&e) else { return };
        let lar = c.lar().unwrap();
        exec_quiet(&e, &c, &["DROP TABLE stlisten"]);
        exec(&e, &c, &[
            "CREATE TABLE stlisten (id BIGINT PRIMARY KEY, v TEXT, n DOUBLE PRECISION, at TIMESTAMPTZ, b BOOLEAN)",
            "INSERT INTO stlisten VALUES (1, 'a', 1.5, '2026-01-01T00:00:00Z', true)",
        ]);
        let l = e.core.block_on(lar.listen("dataman_ch", "SELECT id, v, n, at, b FROM stlisten ORDER BY id")).unwrap();
        wait_until(10, "first notification", || l.received() >= 1);
        let p = l.latest().unwrap();
        eprintln!("LISTEN AS payload: ts={} columns={:?} rows={:?}", p.ts, p.columns, p.rows);
        assert_eq!(p.columns, vec!["id", "v", "n", "at", "b"]);
        assert_eq!(p.rows.len(), 1);
        assert_eq!(p.text(0, 1).as_deref(), Some("a"));
        let t = Instant::now();
        exec(&e, &c, &["INSERT INTO stlisten VALUES (2, NULL, NULL, NULL, NULL)"]);
        wait_until(10, "second notification", || l.received() >= 2);
        eprintln!("LISTEN AS: commit → notification in {:?}", t.elapsed());
        let p = l.latest().unwrap();
        assert_eq!(p.rows.len(), 2);
        assert_eq!(p.text(1, 1), None);
        assert_eq!(l.log().len(), 2);
        assert!(l.log().iter().all(|n| n.error.is_none()));
        e.core.block_on(l.unlisten()).unwrap();
        exec(&e, &c, &["INSERT INTO stlisten VALUES (3, 'c', 3, NULL, NULL)"]);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(l.received(), 2);
        exec(&e, &c, &["DROP TABLE stlisten"]);
    }

    #[test]
    fn subscribe_live_rows_pause_resume_stop() {
        let e = env();
        let Some(c) = lar(&e) else { return };
        let lar = c.lar().unwrap();
        exec_quiet(&e, &c, &["DROP TABLE stsub"]);
        exec(&e, &c, &["CREATE TABLE stsub (id BIGINT PRIMARY KEY, v TEXT, n DOUBLE PRECISION)", "INSERT INTO stsub VALUES (1, 'a', 1), (2, 'b\tx', NULL)"]);
        let s = e.core.block_on(lar.subscribe("SELECT id, v, n FROM stsub", SubscribeOptions::default())).unwrap();
        wait_until(10, "initial result", || s.with_table(|t| t.len()) == 2);
        assert_eq!(s.columns(), vec!["id", "v", "n"]);
        let rows = s.rows();
        assert!(rows.iter().all(|r| r.kind == ChangeKind::Initial));
        let r2 = rows.iter().find(|r| r.values[0].as_deref() == Some("2")).unwrap();
        assert_eq!(r2.values[1].as_deref(), Some("b\tx"));
        assert_eq!(r2.values[2], None);
        let t = Instant::now();
        exec(&e, &c, &["UPDATE stsub SET v = 'A' WHERE id = 1"]);
        wait_until(10, "update", || s.rows().iter().any(|r| r.values[1].as_deref() == Some("A")));
        eprintln!("SUBSCRIBE: commit → delta applied in {:?}", t.elapsed());
        let r1 = s.rows().into_iter().find(|r| r.values[0].as_deref() == Some("1")).unwrap();
        assert_eq!(r1.kind, ChangeKind::Updated { changed: vec![false, true, false] });
        assert_eq!(s.stats().last.updated, 1);

        s.pause();
        assert_eq!(s.state(), SubState::Paused);
        exec(&e, &c, &["INSERT INTO stsub VALUES (3, 'c', 3)", "DELETE FROM stsub WHERE id = 2"]);
        wait_until(10, "buffered deltas", || s.stats().buffered >= 2);
        assert_eq!(s.with_table(|t| t.len()), 2);
        assert!(!s.rows().iter().any(|r| r.values[0].as_deref() == Some("3")));
        s.resume();
        assert_eq!(s.state(), SubState::Live);
        let ids: std::collections::BTreeSet<String> = s.rows().iter().map(|r| r.values[0].clone().unwrap()).collect();
        assert_eq!(ids, ["1", "3"].iter().map(|s| s.to_string()).collect());
        assert_eq!(s.with_table(|t| t.removed.len()), 1);
        let st = s.stats();
        eprintln!("SUBSCRIBE stats: {st:?}");
        assert!(st.deltas >= 6 && st.batches >= 4 && st.buffered == 0);
        let t = Instant::now();
        e.core.block_on(s.stop()).unwrap();
        assert_eq!(s.state(), SubState::Ended { error: None });
        eprintln!("SUBSCRIBE stop (CancelRequest → 57014) in {:?}", t.elapsed());

        let s = e.core.block_on(lar.subscribe("SELECT id, v FROM stsub WHERE n > 100", SubscribeOptions::default())).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(s.with_table(|t| t.len()), 0);
        exec(&e, &c, &["INSERT INTO stsub VALUES (101, 'big', 500)"]);
        wait_until(10, "insert into an empty subscription", || s.with_table(|t| t.len()) == 1);
        assert_eq!(s.rows()[0].kind, ChangeKind::Inserted);
        assert!(!s.paused_by_user());
        s.pause();
        let t = Instant::now();
        while !matches!(s.state(), SubState::Ended { .. }) && t.elapsed() < Duration::from_secs(4) {
            std::thread::sleep(Duration::from_millis(50));
        }
        eprintln!("SUBSCRIBE paused: state after {:?}: {:?}", t.elapsed(), s.state());
        if !matches!(s.state(), SubState::Ended { .. }) {
            e.core.block_on(s.stop()).unwrap();
        }
        assert!(matches!(s.state(), SubState::Ended { .. }));
        assert!(s.paused_by_user());
        drop(s);
        exec(&e, &c, &["DROP TABLE stsub"]);
    }
}
