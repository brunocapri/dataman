#![cfg(target_os = "linux")]

use dataman_core::{Keychain, SecretStore};

#[test]
fn no_secret_service_reads_nothing_and_says_why_on_save() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary; nothing else reads the environment meanwhile.
    unsafe {
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={}", dir.path().join("no-bus").display()));
        std::env::set_var("XDG_RUNTIME_DIR", dir.path());
    }
    let k = Keychain;
    assert_eq!(k.get("dataman-test-none").unwrap(), None);
    k.delete("dataman-test-none").unwrap();
    let err = k.set("dataman-test-none", "pw").unwrap_err();
    assert!(err.downcast_ref::<dataman_core::secret::Unavailable>().is_some(), "{err:#}");
    assert!(err.to_string().contains("no keyring to save passwords in"), "{err:#}");
}
