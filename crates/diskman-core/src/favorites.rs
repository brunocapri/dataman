//! Favorite queries (`~/.config/diskman/favorites.toml`), shared between the app and
//! `diskman-mcp`. Unlike [`crate::history`], favorites are never touched by pruning, limits
//! or "clear history": only an explicit [`FavoritesStore::remove`] drops one. Re-read when the
//! file's modification time changes, so either process sees the other's edits.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::config::Dirs;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Favorite {
    pub id: String,
    pub name: String,
    pub sql: String,
    /// The connection this query was saved from, if any (used to open it in that connection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
}

impl Favorite {
    pub fn new(name: impl Into<String>, sql: impl Into<String>) -> Favorite {
        Favorite { id: uuid::Uuid::new_v4().simple().to_string(), name: name.into(), sql: sql.into(), connection_id: None, database: None }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default, rename = "favorite")]
    favorites: Vec<Favorite>,
}

struct Fresh {
    stamp: Option<SystemTime>,
    items: Vec<Favorite>,
}

pub struct FavoritesStore {
    path: PathBuf,
    state: Mutex<Fresh>,
}

impl FavoritesStore {
    pub fn open(dirs: &Dirs) -> Result<FavoritesStore> {
        let store = FavoritesStore { path: dirs.favorites_file(), state: Mutex::new(Fresh { stamp: None, items: Vec::new() }) };
        store.refresh()?;
        Ok(store)
    }

    fn mtime(&self) -> Option<SystemTime> {
        std::fs::metadata(&self.path).and_then(|m| m.modified()).ok()
    }

    fn refresh(&self) -> Result<()> {
        let stamp = self.mtime();
        let mut g = self.state.lock();
        if stamp.is_some() && g.stamp == stamp {
            return Ok(());
        }
        let items = match std::fs::read_to_string(&self.path) {
            Ok(s) => toml::from_str::<File>(&s).with_context(|| format!("reading {}", self.path.display()))?.favorites,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.path.display())),
        };
        g.stamp = stamp;
        g.items = items;
        Ok(())
    }

    /// Every favorite, re-read from disk first if it changed since the last read.
    pub fn list(&self) -> Vec<Favorite> {
        let _ = self.refresh();
        self.state.lock().items.clone()
    }

    /// Adds a favorite, unless one with the same statement (trimmed) is saved already; returns
    /// the favorite that ended up saved (the new one, or the matching existing one).
    pub fn add(&self, fav: Favorite) -> Result<Favorite> {
        let _ = self.refresh();
        let mut g = self.state.lock();
        if let Some(existing) = g.items.iter().find(|f| f.sql.trim() == fav.sql.trim()) {
            return Ok(existing.clone());
        }
        g.items.push(fav.clone());
        save(&self.path, &g.items)?;
        g.stamp = self.mtime();
        Ok(fav)
    }

    /// Only an explicit call removes a favorite: history pruning and "clear history" never do.
    pub fn remove(&self, id: &str) -> Result<()> {
        let _ = self.refresh();
        let mut g = self.state.lock();
        g.items.retain(|f| f.id != id);
        save(&self.path, &g.items)?;
        g.stamp = self.mtime();
        Ok(())
    }

    /// The favorite whose statement matches `sql` (trimmed), if any.
    pub fn find_by_sql(&self, sql: &str) -> Option<Favorite> {
        let _ = self.refresh();
        self.state.lock().items.iter().find(|f| f.sql.trim() == sql.trim()).cloned()
    }

    /// Renames a favorite in place (its statement, connection and database stay); `None` when
    /// `id` is not found.
    pub fn rename(&self, id: &str, name: &str) -> Result<Option<Favorite>> {
        let _ = self.refresh();
        let mut g = self.state.lock();
        let Some(f) = g.items.iter_mut().find(|f| f.id == id) else { return Ok(None) };
        f.name = name.to_string();
        let updated = f.clone();
        save(&self.path, &g.items)?;
        g.stamp = self.mtime();
        Ok(Some(updated))
    }
}

/// Writes atomically: a temporary file in the same directory, then `rename`.
fn save(path: &Path, items: &[Favorite]) -> Result<()> {
    let dir = path.parent().context("favorites path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let text = toml::to_string_pretty(&File { favorites: items.to_vec() })?;
    let tmp = dir.join(format!(".favorites.toml.{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_dedups_by_content_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("diskman"));
        let store = FavoritesStore::open(&dirs).unwrap();
        assert!(store.list().is_empty());
        let a = store.add(Favorite::new("top customers", "select * from customers")).unwrap();
        let b = store.add(Favorite::new("dup", "  select * from customers  ")).unwrap();
        assert_eq!(a.id, b.id, "same statement, no duplicate");
        assert_eq!(store.list().len(), 1);
        let again = FavoritesStore::open(&dirs).unwrap();
        assert_eq!(again.list(), vec![a.clone()]);
    }

    #[test]
    fn remove_is_the_only_way_out() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("diskman"));
        let store = FavoritesStore::open(&dirs).unwrap();
        let f = store.add(Favorite::new("x", "select 1")).unwrap();
        store.remove(&f.id).unwrap();
        assert!(store.list().is_empty());
        assert!(FavoritesStore::open(&dirs).unwrap().list().is_empty());
    }

    #[test]
    fn rename_keeps_everything_else() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("diskman"));
        let store = FavoritesStore::open(&dirs).unwrap();
        let mut f = Favorite::new("old name", "select 1");
        f.connection_id = Some("c1".into());
        f.database = Some("db1".into());
        let f = store.add(f).unwrap();
        let renamed = store.rename(&f.id, "new name").unwrap().unwrap();
        assert_eq!(renamed.name, "new name");
        assert_eq!((renamed.sql.as_str(), renamed.connection_id.as_deref(), renamed.database.as_deref()), ("select 1", Some("c1"), Some("db1")));
        assert_eq!(store.list(), vec![renamed]);
        assert!(store.rename("nope", "x").unwrap().is_none());
    }

    #[test]
    fn sees_another_process_writing_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("diskman"));
        let store = FavoritesStore::open(&dirs).unwrap();
        assert!(store.list().is_empty());
        let other = FavoritesStore::open(&dirs).unwrap();
        other.add(Favorite::new("x", "select 1")).unwrap();
        assert_eq!(store.list().len(), 1, "re-read after the file's mtime changed");
    }
}
