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

pub const STORE_NAME: &str = if cfg!(target_os = "macos") {
    "Keychain"
} else if cfg!(windows) {
    "Credential Manager"
} else {
    "keyring"
};

#[cfg(any(target_os = "macos", test))]
mod vault;

// Service `dataman`, account = connection id: saved passwords are found by these. On macOS they
// all live in one Keychain item (see `vault`), the older per-connection items moving in when read.
pub struct Keychain;

#[cfg(target_os = "macos")]
fn vault() -> &'static vault::Vault<vault::mac::Keychain> {
    static VAULT: std::sync::OnceLock<vault::Vault<vault::mac::Keychain>> = std::sync::OnceLock::new();
    VAULT.get_or_init(|| {
        let lock = crate::config::Dirs::default_dirs().ok().map(|d| d.config.join("secrets.lock"));
        vault::Vault::new(vault::mac::Keychain, lock)
    })
}

#[cfg(target_os = "macos")]
impl SecretStore for Keychain {
    fn get(&self, id: &str) -> Result<Option<String>> {
        vault().get(id)
    }

    fn set(&self, id: &str, password: &str) -> Result<()> {
        vault().set(id, password)
    }

    fn delete(&self, id: &str) -> Result<()> {
        vault().delete(id)
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

/// Windows' Credential Manager: a generic credential per secret, `dataman:<id>`, the password as
/// UTF-8, saved for this user on this machine. A credential holds at most 2560 bytes: a longer
/// secret (the approved rules of many connections) continues in `dataman:<id>:<n>`, n = 1…, and
/// the first one's comment counts the parts.
#[cfg(windows)]
mod windows {
    use anyhow::{Context as _, Result};
    use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, GetLastError};
    use windows_sys::Win32::Security::Credentials::{CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW, CredWriteW};

    use super::SERVICE;

    pub const PART: usize = 2560; // CRED_MAX_CREDENTIAL_BLOB_SIZE
    const PARTS: &str = "dataman parts: ";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    fn target(id: &str, part: usize) -> String {
        if part == 0 { format!("{SERVICE}:{id}") } else { format!("{SERVICE}:{id}:{part}") }
    }

    fn last_error() -> std::io::Error {
        std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
    }

    /// The blob and the comment; `None`: no such credential.
    fn read(target: &str) -> Result<Option<(Vec<u8>, String)>> {
        let t = wide(target);
        let mut p: *mut CREDENTIALW = std::ptr::null_mut();
        if unsafe { CredReadW(t.as_ptr(), CRED_TYPE_GENERIC, 0, &mut p) } == 0 {
            if unsafe { GetLastError() } == ERROR_NOT_FOUND {
                return Ok(None);
            }
            return Err(last_error()).with_context(|| format!("reading {target} in the Credential Manager"));
        }
        let c = unsafe { &*p };
        let blob = if c.CredentialBlob.is_null() { Vec::new() } else { unsafe { std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize) }.to_vec() };
        let comment = if c.Comment.is_null() {
            String::new()
        } else {
            let n = (0..).take_while(|&i| unsafe { *c.Comment.add(i) } != 0).count();
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(c.Comment, n) })
        };
        unsafe { CredFree(p.cast()) };
        Ok(Some((blob, comment)))
    }

    fn write(target: &str, blob: &[u8], comment: &str, user: &str) -> Result<()> {
        let (mut t, mut c, mut u) = (wide(target), wide(comment), wide(user));
        let mut blob = blob.to_vec();
        let cred = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: t.as_mut_ptr(),
            Comment: c.as_mut_ptr(),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            UserName: u.as_mut_ptr(),
            ..unsafe { std::mem::zeroed() }
        };
        if unsafe { CredWriteW(&cred, 0) } == 0 {
            return Err(last_error()).with_context(|| format!("saving {target} in the Credential Manager"));
        }
        Ok(())
    }

    fn remove(target: &str) -> Result<()> {
        let t = wide(target);
        if unsafe { CredDeleteW(t.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 && unsafe { GetLastError() } != ERROR_NOT_FOUND {
            return Err(last_error()).with_context(|| format!("removing {target} from the Credential Manager"));
        }
        Ok(())
    }

    fn parts(comment: &str) -> usize {
        comment.strip_prefix(PARTS).and_then(|n| n.parse().ok()).unwrap_or(1).max(1)
    }

    /// One call at a time: a secret of several parts is read and written whole, and writes made
    /// at once from two threads were seen lost (two of them, read back at once as missing).
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn one() -> std::sync::MutexGuard<'static, ()> {
        ONE.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get(id: &str) -> Result<Option<String>> {
        let _one = one();
        let Some((mut data, comment)) = read(&target(id, 0))? else { return Ok(None) };
        for n in 1..parts(&comment) {
            let (more, _) = read(&target(id, n))?.with_context(|| format!("part {n} of {} is missing from the Credential Manager", target(id, 0)))?;
            data.extend(more);
        }
        Ok(Some(String::from_utf8(data).context("the saved password is not text")?))
    }

    pub fn set(id: &str, password: &str) -> Result<()> {
        let _one = one();
        let was = read(&target(id, 0))?.map_or(0, |(_, c)| parts(&c));
        let chunks: Vec<&[u8]> = if password.is_empty() { vec![&[]] } else { password.as_bytes().chunks(PART).collect() };
        // The continuations first: the first part, written last, says how many there are.
        for (n, chunk) in chunks.iter().enumerate().skip(1) {
            write(&target(id, n), chunk, "", id)?;
        }
        let comment = if chunks.len() > 1 { format!("{PARTS}{}", chunks.len()) } else { String::new() };
        write(&target(id, 0), chunks[0], &comment, id)?;
        for n in chunks.len()..was {
            remove(&target(id, n))?;
        }
        Ok(())
    }

    pub fn delete(id: &str) -> Result<()> {
        let _one = one();
        let was = read(&target(id, 0))?.map_or(1, |(_, c)| parts(&c));
        remove(&target(id, 0))?;
        for n in 1..was {
            remove(&target(id, n))?;
        }
        Ok(())
    }
}

#[cfg(windows)]
impl SecretStore for Keychain {
    fn get(&self, id: &str) -> Result<Option<String>> {
        windows::get(id)
    }

    fn set(&self, id: &str, password: &str) -> Result<()> {
        windows::set(id, password)
    }

    fn delete(&self, id: &str) -> Result<()> {
        windows::delete(id)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
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
