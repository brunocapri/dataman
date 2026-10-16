//! The panic hook for real (its own test binary: the hook and the logger are process-wide).

#[test]
fn panic_writes_a_report() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("logs");
    dataman_core::crash::install(dir.clone(), "9.9.9");
    log::warn!(target: "dataman::test", "before the panic");
    let h = std::thread::Builder::new().name("worker-7".into()).spawn(|| panic!("deliberate {}", 42)).unwrap();
    assert!(h.join().is_err());
    // A second panic in the same process is appended to the same report.
    let _ = std::thread::spawn(|| panic!("second")).join();
    let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).collect();
    assert_eq!(files.len(), 1, "{files:?}");
    let text = std::fs::read_to_string(&files[0]).unwrap();
    for want in ["Dataman 9.9.9 crash report", "thread    worker-7", "panic     deliberate 42", "crash_hook.rs", "backtrace", "before the panic", "then, on thread"] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
    let p = dataman_core::crash::take_pending(&dir, &tmp.path().join("none")).unwrap();
    assert_eq!(p.report.as_deref(), Some(files[0].as_path()));
}
