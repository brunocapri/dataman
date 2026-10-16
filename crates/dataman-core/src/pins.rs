//! Pinned tables (`~/.config/dataman/pins.toml`): tables and views the explorer keeps in a
//! "pinned" group at the top of their connection, by connection, database, schema and name, in
//! the order they were pinned. The file exists only while something is pinned; connections,
//! and the files of older versions, are untouched by it.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::config::Dirs;
use crate::schema::RelKind;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pin {
    /// The connection's id.
    pub connection: String,
    pub database: String,
    /// The database's name again where databases have no schemas (MySQL).
    pub schema: String,
    pub name: String,
    /// What it was when pinned (`RelKind::label`: `table`, `view`…), for its icon and menu
    /// before its schema is loaded; empty or unknown reads as a table.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
}

impl Pin {
    pub fn new(connection: &str, database: &str, schema: &str, name: &str, kind: RelKind) -> Pin {
        Pin { connection: connection.into(), database: database.into(), schema: schema.into(), name: name.into(), kind: kind.label().into() }
    }

    pub fn kind(&self) -> RelKind {
        [RelKind::Table, RelKind::View, RelKind::MaterializedView, RelKind::Foreign, RelKind::Partitioned]
            .into_iter()
            .find(|k| k.label() == self.kind)
            .unwrap_or(RelKind::Table)
    }

    /// The same relation (its kind aside).
    pub fn is(&self, connection: &str, database: &str, schema: &str, name: &str) -> bool {
        self.connection == connection && self.database == database && self.schema == schema && self.name == name
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default, rename = "pin")]
    pins: Vec<Pin>,
}

pub struct PinStore {
    path: PathBuf,
    pins: Mutex<Vec<Pin>>,
}

impl PinStore {
    /// The pins saved in `dirs`. A file that does not read (a hand edit half done) is logged
    /// and leaves none: pins are a convenience, never a reason not to start.
    pub fn open(dirs: &Dirs) -> PinStore {
        let path = dirs.pins_file();
        let pins = read(&path).unwrap_or_else(|e| {
            log::warn!("{e:#}");
            Vec::new()
        });
        PinStore { path, pins: Mutex::new(pins) }
    }

    /// A connection's pins, in the order they were pinned.
    pub fn of(&self, connection: &str) -> Vec<Pin> {
        self.pins.lock().iter().filter(|p| p.connection == connection).cloned().collect()
    }

    pub fn is_pinned(&self, connection: &str, database: &str, schema: &str, name: &str) -> bool {
        self.pins.lock().iter().any(|p| p.is(connection, database, schema, name))
    }

    /// Pins a relation (last of its connection's; already pinned: nothing changes) and saves.
    pub fn pin(&self, pin: Pin) -> Result<()> {
        let mut g = self.pins.lock();
        if g.iter().any(|p| p.is(&pin.connection, &pin.database, &pin.schema, &pin.name)) {
            return Ok(());
        }
        g.push(pin);
        save(&self.path, &g)
    }

    /// Unpins a relation and saves (the file goes with the last pin).
    pub fn unpin(&self, connection: &str, database: &str, schema: &str, name: &str) -> Result<()> {
        let mut g = self.pins.lock();
        let n = g.len();
        g.retain(|p| !p.is(connection, database, schema, name));
        if g.len() == n {
            return Ok(());
        }
        save(&self.path, &g)
    }

    /// A connection's pins replaced by `pins` (in that order), the others' kept: what the app's
    /// undo puts back.
    pub fn set_of(&self, connection: &str, pins: &[Pin]) -> Result<()> {
        let mut g = self.pins.lock();
        let at = g.iter().position(|p| p.connection == connection).unwrap_or(g.len());
        let mut next: Vec<Pin> = g.iter().filter(|p| p.connection != connection).cloned().collect();
        let at = at.min(next.len());
        next.splice(at..at, pins.iter().filter(|p| p.connection == connection).cloned());
        if next == *g {
            return Ok(());
        }
        *g = next;
        save(&self.path, &g)
    }

    /// Drops every pin of a connection (deleted).
    pub fn forget(&self, connection: &str) -> Result<()> {
        let mut g = self.pins.lock();
        let n = g.len();
        g.retain(|p| p.connection != connection);
        if g.len() == n {
            return Ok(());
        }
        save(&self.path, &g)
    }
}

fn read(path: &Path) -> Result<Vec<Pin>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(toml::from_str::<File>(&s).with_context(|| format!("reading {}", path.display()))?.pins),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes atomically (a temporary file in the same directory, then `rename`); no pins, no file.
fn save(path: &Path, pins: &[Pin]) -> Result<()> {
    if pins.is_empty() {
        return match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).with_context(|| format!("removing {}", path.display())),
            _ => Ok(()),
        };
    }
    let dir = path.parent().context("pins path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let text = toml::to_string_pretty(&File { pins: pins.to_vec() })?;
    let tmp = dir.join(format!(".pins.toml.{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Dirs, PinStore) {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        let store = PinStore::open(&dirs);
        (dir, dirs, store)
    }

    #[test]
    fn pins_round_trip_per_connection_in_order() {
        let (_d, dirs, s) = store();
        assert!(s.of("c1").is_empty());
        s.pin(Pin::new("c1", "app", "public", "orders", RelKind::Table)).unwrap();
        s.pin(Pin::new("c2", "shop", "shop", "items", RelKind::Table)).unwrap();
        s.pin(Pin::new("c1", "app", "sales", "daily", RelKind::View)).unwrap();
        // Pinned again: no duplicate, its place kept.
        s.pin(Pin::new("c1", "app", "public", "orders", RelKind::Table)).unwrap();
        let names = |v: Vec<Pin>| v.into_iter().map(|p| p.name).collect::<Vec<_>>();
        assert_eq!(names(s.of("c1")), ["orders", "daily"]);
        assert_eq!(names(s.of("c2")), ["items"]);
        assert!(s.is_pinned("c1", "app", "sales", "daily"));
        // Database and schema are part of the key.
        assert!(!s.is_pinned("c1", "other", "sales", "daily") && !s.is_pinned("c1", "app", "public", "daily"));
        let again = PinStore::open(&dirs);
        assert_eq!(again.of("c1"), s.of("c1"));
        assert_eq!(again.of("c1")[1].kind(), RelKind::View);
    }

    #[test]
    fn unpinning_the_last_removes_the_file() {
        let (_d, dirs, s) = store();
        s.unpin("c1", "app", "public", "orders").unwrap();
        assert!(!dirs.pins_file().exists(), "nothing pinned: no file");
        s.pin(Pin::new("c1", "app", "public", "orders", RelKind::Table)).unwrap();
        s.pin(Pin::new("c1", "app", "public", "items", RelKind::Table)).unwrap();
        let text = std::fs::read_to_string(dirs.pins_file()).unwrap();
        assert_eq!(text.matches("[[pin]]").count(), 2, "{text}");
        assert!(text.contains("kind = \"table\""), "{text}");
        s.unpin("c1", "app", "public", "orders").unwrap();
        assert_eq!(PinStore::open(&dirs).of("c1").len(), 1);
        s.unpin("c1", "app", "public", "items").unwrap();
        assert!(!dirs.pins_file().exists());
        assert!(PinStore::open(&dirs).of("c1").is_empty());
    }

    #[test]
    fn forgetting_a_connection() {
        let (_d, dirs, s) = store();
        s.pin(Pin::new("c1", "app", "public", "orders", RelKind::Table)).unwrap();
        s.pin(Pin::new("c2", "app", "public", "orders", RelKind::Table)).unwrap();
        s.forget("c1").unwrap();
        assert!(s.of("c1").is_empty() && s.of("c2").len() == 1);
        assert!(PinStore::open(&dirs).of("c1").is_empty());
    }

    /// A connection's pins put back as they were (the undo), the other connections' untouched;
    /// none left, no file.
    #[test]
    fn setting_a_connections_pins() {
        let (_d, dirs, s) = store();
        s.pin(Pin::new("c1", "app", "public", "a", RelKind::Table)).unwrap();
        s.pin(Pin::new("c2", "app", "public", "x", RelKind::Table)).unwrap();
        s.pin(Pin::new("c1", "app", "public", "b", RelKind::Table)).unwrap();
        let before = s.of("c1");
        s.unpin("c1", "app", "public", "a").unwrap();
        s.set_of("c1", &before).unwrap();
        let names = |v: Vec<Pin>| v.into_iter().map(|p| p.name).collect::<Vec<_>>();
        assert_eq!(names(PinStore::open(&dirs).of("c1")), ["a", "b"]);
        assert_eq!(names(s.of("c2")), ["x"]);
        s.set_of("c1", &[]).unwrap();
        s.set_of("c2", &[]).unwrap();
        assert!(!dirs.pins_file().exists());
    }

    /// The kind is optional (a hand-written pin is a table) and an unknown one reads as a
    /// table; a broken file starts with no pins instead of failing.
    #[test]
    fn lenient_reading() {
        let (_d, dirs, _s) = store();
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::fs::write(dirs.pins_file(), "[[pin]]\nconnection = \"c1\"\ndatabase = \"d\"\nschema = \"s\"\nname = \"t\"\n\n[[pin]]\nconnection = \"c1\"\ndatabase = \"d\"\nschema = \"s\"\nname = \"m\"\nkind = \"materialized view\"\n\n[[pin]]\nconnection = \"c1\"\ndatabase = \"d\"\nschema = \"s\"\nname = \"x\"\nkind = \"hypertable\"\n").unwrap();
        let kinds: Vec<RelKind> = PinStore::open(&dirs).of("c1").iter().map(Pin::kind).collect();
        assert_eq!(kinds, [RelKind::Table, RelKind::MaterializedView, RelKind::Table]);
        std::fs::write(dirs.pins_file(), "[[pin]]\nconnection = ").unwrap();
        assert!(PinStore::open(&dirs).of("c1").is_empty());
    }
}
