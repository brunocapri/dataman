//! Password storage: the macOS Keychain (via `keyring`), or memory in tests.

use std::collections::HashMap;

use anyhow::Result;
use parking_lot::Mutex;

pub trait SecretStore: Send + Sync {
    fn get(&self, connection_id: &str) -> Result<Option<String>>;
    fn set(&self, connection_id: &str, password: &str) -> Result<()>;
    fn delete(&self, connection_id: &str) -> Result<()>;
}

pub const SERVICE: &str = "dataman";

/// The system keychain: service `dataman`, account = connection id.
pub struct Keychain;

impl SecretStore for Keychain {
    fn get(&self, id: &str) -> Result<Option<String>> {
        match keyring::Entry::new(SERVICE, id)?.get_password() {
            Ok(p) => Ok(Some(p)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn set(&self, id: &str, password: &str) -> Result<()> {
        Ok(keyring::Entry::new(SERVICE, id)?.set_password(password)?)
    }

    fn delete(&self, id: &str) -> Result<()> {
        match keyring::Entry::new(SERVICE, id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[derive(Default)]
pub struct MemorySecrets(Mutex<HashMap<String, String>>);

impl SecretStore for MemorySecrets {
    fn get(&self, id: &str) -> Result<Option<String>> {
        Ok(self.0.lock().get(id).cloned())
    }
    fn set(&self, id: &str, password: &str) -> Result<()> {
        self.0.lock().insert(id.into(), password.into());
        Ok(())
    }
    fn delete(&self, id: &str) -> Result<()> {
        self.0.lock().remove(id);
        Ok(())
    }
}
