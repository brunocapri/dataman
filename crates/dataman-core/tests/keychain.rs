//! Ignored by default: it touches the real login keychain.

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

#[test]
#[ignore]
fn keychain_entries_are_per_connection() {
    let (a, b) = (format!("dataman-test-a-{}", std::process::id()), format!("dataman-test-b-{}", std::process::id()));
    let k = Keychain;
    k.set(&a, "one").unwrap();
    k.set(&b, "two").unwrap();
    assert_eq!(k.get(&a).unwrap().as_deref(), Some("one"));
    assert_eq!(k.get(&b).unwrap().as_deref(), Some("two"));
    k.delete(&a).unwrap();
    assert_eq!(k.get(&a).unwrap(), None);
    assert_eq!(k.get(&b).unwrap().as_deref(), Some("two"));
    k.delete(&b).unwrap();
}
