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

/// The agent login's password: its own account next to the connection's, read the same way
/// the MCP server reads it (`password_of`), and gone with Remove.
#[test]
#[ignore]
fn agent_login_password_in_the_keychain() {
    use dataman_core::agent_login::{live, secret_account};
    let id = format!("dataman-test-conn-{}", std::process::id());
    let k = Keychain;
    k.set(&id, "the-users-password").unwrap();
    assert!(live::password_of(&k, &id, "dataman_agent_x").is_err(), "the user's password is never the agent's");
    k.set(&secret_account(&id), "the-agents-password").unwrap();
    assert_eq!(live::password_of(&k, &id, "dataman_agent_x").unwrap(), "the-agents-password");
    assert_eq!(k.get(&id).unwrap().as_deref(), Some("the-users-password"));
    k.delete(&secret_account(&id)).unwrap();
    assert!(live::password_of(&k, &id, "dataman_agent_x").is_err());
    k.delete(&id).unwrap();
}
