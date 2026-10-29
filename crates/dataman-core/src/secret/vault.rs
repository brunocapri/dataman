//! On macOS every saved secret is one entry of a single Keychain item, so a process that is not
//! in the item's access list asks once, not once per connection. The app and each `dataman-mcp`
//! read and write the same item: a process keeps what it read, and before using it checks the
//! item's attributes (a fresh tag written with every change, and the modification date), which
//! the Keychain gives without the secret and without asking. Writes are read-modify-write under a
//! lock file shared by every Dataman process, so two of them never lose each other's change.
//!
//! The items of older versions (service `dataman`, account = the secret's id) move in when read,
//! and go once the vault holds what they held.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::SERVICE;

pub const VAULT_SERVICE: &str = "dataman.vault";
pub const VAULT_ACCOUNT: &str = "secrets";
const VERSION: u32 = 1;

/// The Keychain's generic passwords, by service and account. Not found is `None`; anything else
/// the Keychain says (a refused or cancelled prompt among them) is an error.
pub trait Raw: Send + Sync {
    fn read(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>>;
    /// Changes with every `write`; read from the attributes only, never the secret.
    fn stamp(&self, service: &str, account: &str) -> Result<Option<String>>;
    fn write(&self, service: &str, account: &str, data: &[u8]) -> Result<()>;
    /// Gone already is fine.
    fn remove(&self, service: &str, account: &str) -> Result<()>;
}

#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    secrets: BTreeMap<String, String>,
}

/// What this process last read, and the stamp it was read at; `None`: no vault item.
struct Read {
    stamp: Option<String>,
    secrets: BTreeMap<String, String>,
}

pub struct Vault<R> {
    raw: R,
    lock: Option<PathBuf>,
    // Held through each read too, so this process's threads ask at most once between them.
    read: Mutex<Option<Read>>,
}

impl<R: Raw> Vault<R> {
    /// `lock`: the file every process writing this vault locks; none (no home): nothing is written.
    pub fn new(raw: R, lock: Option<PathBuf>) -> Vault<R> {
        Vault { raw, lock, read: Mutex::new(None) }
    }

    pub fn get(&self, id: &str) -> Result<Option<String>> {
        let mut read = self.read.lock();
        if let Some(s) = self.current(&mut read)?.get(id) {
            return Ok(Some(s.clone()));
        }
        let Some(legacy) = self.raw.read(SERVICE, id)? else { return Ok(None) };
        let legacy = String::from_utf8(legacy).context("the saved password is not text")?;
        // Moved in, unless another process saved one for it meanwhile: that one is newer.
        let mut kept = legacy.clone();
        let moved = self.change(&mut read, |m| kept = m.entry(id.into()).or_insert(legacy).clone());
        match moved {
            Ok(()) => {
                if let Err(e) = self.raw.remove(SERVICE, id) {
                    log::warn!("the Keychain item of {id} stays after moving into Dataman's: {e:#}");
                }
            }
            Err(e) => log::warn!("{id} stays in its own Keychain item: {e:#}"),
        }
        Ok(Some(kept))
    }

    pub fn set(&self, id: &str, secret: &str) -> Result<()> {
        let mut read = self.read.lock();
        self.change(&mut read, |m| {
            m.insert(id.into(), secret.into());
        })?;
        if let Err(e) = self.raw.remove(SERVICE, id) {
            log::warn!("the older Keychain item of {id} stays (the new one is read first): {e:#}");
        }
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let mut read = self.read.lock();
        self.change(&mut read, |m| {
            m.remove(id);
        })?;
        self.raw.remove(SERVICE, id)
    }

    /// The vault as it is now: what was read before when its stamp has not moved.
    fn current<'a>(&self, read: &'a mut Option<Read>) -> Result<&'a BTreeMap<String, String>> {
        let stamp = self.raw.stamp(VAULT_SERVICE, VAULT_ACCOUNT)?;
        if read.as_ref().is_none_or(|r| r.stamp != stamp) {
            // Read after the stamp: a write in between is newer than the stamp kept, so the next
            // look reads again, never the other way round.
            let secrets = match &stamp {
                None => BTreeMap::new(),
                Some(_) => match self.raw.read(VAULT_SERVICE, VAULT_ACCOUNT)? {
                    None => BTreeMap::new(),
                    Some(data) => parse(&data)?,
                },
            };
            *read = Some(Read { stamp, secrets });
        }
        Ok(&read.as_ref().expect("just read").secrets)
    }

    /// Read-modify-write under the lock of every Dataman process; written only when it changed.
    fn change(&self, read: &mut Option<Read>, f: impl FnOnce(&mut BTreeMap<String, String>)) -> Result<()> {
        let _held = self.hold()?;
        let mut secrets = self.current(read)?.clone();
        let before = secrets.clone();
        f(&mut secrets);
        if secrets == before {
            return Ok(());
        }
        let data = serde_json::to_vec(&Stored { version: VERSION, secrets: secrets.clone() })?;
        if let Err(e) = self.raw.write(VAULT_SERVICE, VAULT_ACCOUNT, &data) {
            // What the Keychain holds now is not known: read it again next time.
            *read = None;
            return Err(e.context("saving in Dataman's Keychain item"));
        }
        // Under the lock nobody else wrote since: this stamp is of these secrets.
        *read = match self.raw.stamp(VAULT_SERVICE, VAULT_ACCOUNT) {
            Ok(stamp @ Some(_)) => Some(Read { stamp, secrets }),
            _ => None,
        };
        Ok(())
    }

    fn hold(&self) -> Result<std::fs::File> {
        let path = self.lock.as_ref().context("no home directory to keep Dataman's lock file in")?;
        if let Some(dir) = path.parent() {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
        let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
        f.lock()?;
        Ok(f)
    }
}

/// An item that does not read is an error, never an empty vault to write over.
fn parse(data: &[u8]) -> Result<BTreeMap<String, String>> {
    let stored: Stored = serde_json::from_slice(data).context("Dataman's Keychain item does not read: it is left as it is")?;
    anyhow::ensure!(stored.version == VERSION, "Dataman's Keychain item is of another version ({}): it is left as it is", stored.version);
    Ok(stored.secrets)
}

#[cfg(target_os = "macos")]
pub mod mac {
    use anyhow::Result;
    use core_foundation::data::CFData;
    use security_framework::base::Error;
    use security_framework::item::{ItemAddOptions, ItemAddValue, ItemClass, ItemSearchOptions, ItemUpdateOptions, ItemUpdateValue, SearchResult, update_item};
    use security_framework::passwords::{PasswordOptions, delete_generic_password, generic_password};

    use super::Raw;

    const NOT_FOUND: i32 = -25300; // errSecItemNotFound
    const DUPLICATE: i32 = -25299; // errSecDuplicateItem

    pub struct Keychain;

    fn found<T>(r: std::result::Result<T, Error>) -> Result<Option<T>> {
        match r {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.code() == NOT_FOUND => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn search(service: &str, account: &str) -> ItemSearchOptions {
        let mut s = ItemSearchOptions::new();
        s.class(ItemClass::generic_password()).service(service).account(account);
        s
    }

    impl Raw for Keychain {
        fn read(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>> {
            found(generic_password(PasswordOptions::new_generic_password(service, account)))
        }

        fn stamp(&self, service: &str, account: &str) -> Result<Option<String>> {
            let Some(items) = found(search(service, account).load_attributes(true).search())? else { return Ok(None) };
            let attrs = items.first().and_then(SearchResult::simplify_dict).unwrap_or_default();
            let at = |k: &str| attrs.get(k).map_or("", String::as_str).to_owned();
            // icmt: the comment, a new tag each write; mdat: the modification date.
            Ok(Some(format!("{} {}", at("icmt"), at("mdat"))))
        }

        fn write(&self, service: &str, account: &str, data: &[u8]) -> Result<()> {
            let tag = uuid::Uuid::new_v4().simple().to_string();
            let value = ItemAddValue::Data { class: ItemClass::generic_password(), data: CFData::from_buffer(data) };
            let mut add = ItemAddOptions::new(value);
            add.set_service(service).set_account_name(account).set_label("Dataman").set_comment(&tag);
            match add.add() {
                Err(e) if e.code() == DUPLICATE => {
                    let mut update = ItemUpdateOptions::new();
                    update.set_value(ItemUpdateValue::Data(CFData::from_buffer(data))).set_comment(&tag);
                    Ok(update_item(&search(service, account), &update)?)
                }
                r => Ok(r?),
            }
        }

        fn remove(&self, service: &str, account: &str) -> Result<()> {
            found(delete_generic_password(service, account)).map(drop)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use super::*;

    /// The Keychain in memory, shared by every "process" (vault) made over it.
    #[derive(Default)]
    struct Fake {
        items: Mutex<HashMap<(String, String), (Vec<u8>, u64)>>,
        writes: AtomicU64,
        reads: AtomicU64,
        deny_reads: AtomicBool,
        deny_writes: AtomicBool,
    }

    #[derive(Clone, Default)]
    struct Shared(Arc<Fake>);

    fn key(s: &str, a: &str) -> (String, String) {
        (s.into(), a.into())
    }

    impl Raw for Shared {
        fn read(&self, s: &str, a: &str) -> Result<Option<Vec<u8>>> {
            anyhow::ensure!(!self.0.deny_reads.load(Ordering::SeqCst), "User canceled the operation.");
            self.0.reads.fetch_add(1, Ordering::SeqCst);
            Ok(self.0.items.lock().get(&key(s, a)).map(|(d, _)| d.clone()))
        }
        fn stamp(&self, s: &str, a: &str) -> Result<Option<String>> {
            Ok(self.0.items.lock().get(&key(s, a)).map(|(_, n)| n.to_string()))
        }
        fn write(&self, s: &str, a: &str, data: &[u8]) -> Result<()> {
            anyhow::ensure!(!self.0.deny_writes.load(Ordering::SeqCst), "User canceled the operation.");
            // A window for a writer that did not hold the lock to lose another's change.
            std::thread::yield_now();
            let n = self.0.writes.fetch_add(1, Ordering::SeqCst) + 1;
            self.0.items.lock().insert(key(s, a), (data.to_vec(), n));
            Ok(())
        }
        fn remove(&self, s: &str, a: &str) -> Result<()> {
            self.0.items.lock().remove(&key(s, a));
            Ok(())
        }
    }

    impl Shared {
        fn legacy(&self, id: &str) -> Option<String> {
            self.0.items.lock().get(&key(SERVICE, id)).map(|(d, _)| String::from_utf8(d.clone()).unwrap())
        }
        fn put_legacy(&self, id: &str, pw: &str) {
            self.0.items.lock().insert(key(SERVICE, id), (pw.as_bytes().to_vec(), 0));
        }
        fn vault(&self) -> Option<Vec<u8>> {
            self.0.items.lock().get(&key(VAULT_SERVICE, VAULT_ACCOUNT)).map(|(d, _)| d.clone())
        }
    }

    fn two(dir: &tempfile::TempDir) -> (Shared, Vault<Shared>, Vault<Shared>) {
        let k = Shared::default();
        let lock = dir.path().join("secrets.lock");
        (k.clone(), Vault::new(k.clone(), Some(lock.clone())), Vault::new(k, Some(lock)))
    }

    #[test]
    fn set_get_delete() {
        let dir = tempfile::tempdir().unwrap();
        let (k, v, _) = two(&dir);
        assert_eq!(v.get("a").unwrap(), None);
        assert_eq!(k.vault(), None, "nothing read, nothing written");
        v.set("a", "one naïve").unwrap();
        v.set("b", "two").unwrap();
        assert_eq!(v.get("a").unwrap().as_deref(), Some("one naïve"));
        v.set("a", "uno").unwrap();
        assert_eq!(v.get("a").unwrap().as_deref(), Some("uno"));
        v.delete("a").unwrap();
        assert_eq!(v.get("a").unwrap(), None);
        assert_eq!(v.get("b").unwrap().as_deref(), Some("two"));
        v.delete("a").unwrap();
        assert_eq!(k.0.items.lock().len(), 1, "one item holds them all");
    }

    #[test]
    fn a_legacy_item_moves_in_when_read() {
        let dir = tempfile::tempdir().unwrap();
        let (k, v, w) = two(&dir);
        k.put_legacy("conn", "old");
        assert_eq!(v.get("conn").unwrap().as_deref(), Some("old"));
        assert_eq!(k.legacy("conn"), None, "gone once the vault holds it");
        assert_eq!(w.get("conn").unwrap().as_deref(), Some("old"), "and the other process finds it there");
    }

    #[test]
    fn a_legacy_item_stays_when_the_vault_is_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let (k, v, _) = two(&dir);
        k.put_legacy("conn", "old");
        k.0.deny_writes.store(true, Ordering::SeqCst);
        assert_eq!(v.get("conn").unwrap().as_deref(), Some("old"));
        assert_eq!(k.legacy("conn").as_deref(), Some("old"));
        k.0.deny_writes.store(false, Ordering::SeqCst);
        assert_eq!(v.get("conn").unwrap().as_deref(), Some("old"));
        assert_eq!(k.legacy("conn"), None);
    }

    #[test]
    fn set_and_delete_take_the_legacy_item_too() {
        let dir = tempfile::tempdir().unwrap();
        let (k, v, _) = two(&dir);
        k.put_legacy("a", "old");
        v.set("a", "new").unwrap();
        assert_eq!(k.legacy("a"), None);
        assert_eq!(v.get("a").unwrap().as_deref(), Some("new"));
        k.put_legacy("b", "old");
        v.delete("b").unwrap();
        assert_eq!(k.legacy("b"), None);
        assert_eq!(v.get("b").unwrap(), None);
    }

    #[test]
    fn a_rotation_in_one_process_is_what_the_other_reads_next() {
        let dir = tempfile::tempdir().unwrap();
        let (k, app, mcp) = two(&dir);
        app.set("agent:c", "first").unwrap();
        assert_eq!(mcp.get("agent:c").unwrap().as_deref(), Some("first"));
        let reads = k.0.reads.load(Ordering::SeqCst);
        assert_eq!(mcp.get("agent:c").unwrap().as_deref(), Some("first"));
        assert_eq!(k.0.reads.load(Ordering::SeqCst), reads, "unchanged: no second read of the secret");
        mcp.set("agent:c", "second").unwrap();
        assert_eq!(app.get("agent:c").unwrap().as_deref(), Some("second"));
        app.delete("agent:c").unwrap();
        assert_eq!(mcp.get("agent:c").unwrap(), None);
    }

    #[test]
    fn concurrent_writers_lose_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (k, a, b) = two(&dir);
        let (a, b) = (Arc::new(a), Arc::new(b));
        let run = |v: Arc<Vault<Shared>>, p: &'static str| {
            std::thread::spawn(move || {
                for i in 0..40 {
                    v.set(&format!("{p}{i}"), &i.to_string()).unwrap();
                }
            })
        };
        let threads = [run(a.clone(), "a"), run(b.clone(), "b"), run(a.clone(), "c")];
        threads.into_iter().for_each(|t| t.join().unwrap());
        let fresh = Vault::new(k, None);
        for p in ["a", "b", "c"] {
            for i in 0..40 {
                assert_eq!(fresh.get(&format!("{p}{i}")).unwrap(), Some(i.to_string()), "{p}{i}");
            }
        }
    }

    #[test]
    fn a_refused_prompt_is_an_error_and_wipes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (k, v, w) = two(&dir);
        v.set("a", "one").unwrap();
        let before = k.vault();
        k.0.deny_reads.store(true, Ordering::SeqCst);
        assert!(w.get("a").is_err(), "not read as no password");
        assert!(w.set("b", "two").is_err());
        assert!(w.delete("a").is_err());
        k.0.deny_reads.store(false, Ordering::SeqCst);
        assert_eq!(k.vault(), before);
        k.0.deny_writes.store(true, Ordering::SeqCst);
        assert!(v.set("a", "changed").is_err());
        k.0.deny_writes.store(false, Ordering::SeqCst);
        assert_eq!(w.get("a").unwrap().as_deref(), Some("one"));
        assert_eq!(v.get("a").unwrap().as_deref(), Some("one"));
    }

    #[test]
    fn a_vault_that_does_not_read_is_never_written_over() {
        let dir = tempfile::tempdir().unwrap();
        let (k, v, _) = two(&dir);
        k.0.items.lock().insert(key(VAULT_SERVICE, VAULT_ACCOUNT), (b"not json".to_vec(), 7));
        k.put_legacy("a", "old");
        assert!(v.get("a").is_err());
        assert!(v.set("b", "two").is_err());
        assert!(v.delete("a").is_err());
        assert_eq!(k.vault().as_deref(), Some(&b"not json"[..]));
        assert_eq!(k.legacy("a").as_deref(), Some("old"));
    }

    #[test]
    fn no_lock_file_no_write() {
        let k = Shared::default();
        let v = Vault::new(k.clone(), None);
        assert!(v.set("a", "one").is_err());
        assert_eq!(k.vault(), None);
    }
}
