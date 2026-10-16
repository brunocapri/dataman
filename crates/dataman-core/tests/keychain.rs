//! The real macOS Keychain. Ignored by default (it touches the login keychain); run with
//! `cargo test --release -p dataman-core --test keychain -- --ignored`.

use dataman_core::{Keychain, SecretStore};

#[test]
#[ignore]
fn keychain_roundtrip() {
    let id = format!("dataman-test-{}", std::process::id());
    let k = Keychain;
    assert_eq!(k.get(&id).unwrap(), None);
    k.set(&id, "s3cr3t naïve").unwrap();
    assert_eq!(k.get(&id).unwrap().as_deref(), Some("s3cr3t naïve"));
    k.set(&id, "other").unwrap();
    assert_eq!(k.get(&id).unwrap().as_deref(), Some("other"));
    k.delete(&id).unwrap();
    assert_eq!(k.get(&id).unwrap(), None);
    k.delete(&id).unwrap();
}
