use std::collections::HashMap;

use anyhow::Result;
use parking_lot::Mutex;

pub trait SecretStore: Send + Sync {
    fn get(&self, connection_id: &str) -> Result<Option<String>>;
    fn set(&self, connection_id: &str, password: &str) -> Result<()>;
    fn delete(&self, connection_id: &str) -> Result<()>;
    /// What is set stays after this process: the rules approved in Dataman can be kept here.
    fn durable(&self) -> bool {
        true
    }
}

pub const SERVICE: &str = "dataman";

pub const STORE_NAME: &str = if cfg!(target_os = "macos") { "Keychain" } else { "keyring" };

// Service `dataman`, account = connection id: saved passwords are found by these.
pub struct Keychain;

#[cfg(target_os = "macos")]
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

/// No Secret Service on the session bus: nothing can be saved, so nothing is there to read
/// or delete either.
#[derive(Debug)]
pub struct Unavailable(pub String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no keyring to save passwords in: start GNOME Keyring, KWallet or KeePassXC's Secret Service, or connect without saving the password ({})", self.0)
    }
}

impl std::error::Error for Unavailable {}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    use anyhow::{Context as _, Result};

    use super::{SERVICE, Unavailable};

    static TOLD: AtomicBool = AtomicBool::new(false);

    fn attributes(id: &str) -> HashMap<&str, &str> {
        HashMap::from([("service", SERVICE), ("account", id)])
    }

    async fn keyring() -> Result<oo7::Keyring> {
        let k = oo7::Keyring::new().await.map_err(|e| Unavailable(e.to_string()))?;
        k.unlock().await.context("the keyring stayed locked")?;
        Ok(k)
    }

    fn missing(e: &anyhow::Error) -> bool {
        if e.downcast_ref::<Unavailable>().is_some() {
            if !TOLD.swap(true, Ordering::Relaxed) {
                log::warn!("{e:#}");
            }
            return true;
        }
        false
    }

    pub fn get(id: &str) -> Result<Option<String>> {
        let r = futures_lite::future::block_on(async {
            let k = keyring().await?;
            let Some(item) = k.search_items(&attributes(id)).await?.into_iter().next() else { return Ok(None) };
            let secret = item.secret().await?;
            Ok(Some(String::from_utf8(secret.as_bytes().to_vec()).context("the saved password is not text")?))
        });
        match r {
            Err(e) if missing(&e) => Ok(None),
            r => r,
        }
    }

    pub fn set(id: &str, password: &str) -> Result<()> {
        futures_lite::future::block_on(async {
            let k = keyring().await?;
            k.create_item(&format!("Dataman connection {id}"), &attributes(id), oo7::Secret::text(password), true).await?;
            Ok(())
        })
    }

    pub fn delete(id: &str) -> Result<()> {
        let r = futures_lite::future::block_on(async {
            keyring().await?.delete(&attributes(id)).await?;
            anyhow::Ok(())
        });
        match r {
            Err(e) if missing(&e) => Ok(()),
            r => r,
        }
    }
}

#[cfg(target_os = "linux")]
impl SecretStore for Keychain {
    fn get(&self, id: &str) -> Result<Option<String>> {
        linux::get(id)
    }

    fn set(&self, id: &str, password: &str) -> Result<()> {
        linux::set(id, password)
    }

    fn delete(&self, id: &str) -> Result<()> {
        linux::delete(id)
    }

    fn durable(&self) -> bool {
        futures_lite::future::block_on(oo7::Keyring::new()).is_ok()
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl SecretStore for Keychain {
    fn get(&self, _: &str) -> Result<Option<String>> {
        Ok(None)
    }

    fn set(&self, _: &str, _: &str) -> Result<()> {
        Err(Unavailable(std::env::consts::OS.into()).into())
    }

    fn delete(&self, _: &str) -> Result<()> {
        Ok(())
    }

    fn durable(&self) -> bool {
        false
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
    fn durable(&self) -> bool {
        false
    }
}
