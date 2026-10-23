//! Folders are optional in the file: one written before folders existed loads as is
//! (everything at the top level), and one without folders is written the same way as before.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::config::Dirs;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Favorite {
    pub id: String,
    pub name: String,
    pub sql: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
}

impl Favorite {
    pub fn new(name: impl Into<String>, sql: impl Into<String>) -> Favorite {
        Favorite { id: uuid::Uuid::new_v4().simple().to_string(), name: name.into(), sql: sql.into(), connection_id: None, database: None, folder: None }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovedFolder {
    pub index: usize,
    pub folder: Folder,
    pub favorites: Vec<(usize, Favorite)>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    /// The `[[folder]]` tables are in the user's order (written with any folder); without it
    /// they load by name.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    folders_ordered: bool,
    #[serde(default, rename = "folder", skip_serializing_if = "Vec::is_empty")]
    folders: Vec<Folder>,
    #[serde(default, rename = "favorite")]
    favorites: Vec<Favorite>,
}

struct Fresh {
    stamp: Option<crate::stamp::FileStamp>,
    items: Vec<Favorite>,
    folders: Vec<Folder>,
}

impl Fresh {
    fn save(&mut self, path: &Path) -> Result<()> {
        save(path, &self.items, &self.folders)?;
        self.stamp = crate::stamp::FileStamp::of(path);
        Ok(())
    }
}

pub struct FavoritesStore {
    path: PathBuf,
    state: Mutex<Fresh>,
}

impl FavoritesStore {
    pub fn open(dirs: &Dirs) -> Result<FavoritesStore> {
        let store = FavoritesStore { path: dirs.favorites_file(), state: Mutex::new(Fresh { stamp: None, items: Vec::new(), folders: Vec::new() }) };
        store.refresh()?;
        Ok(store)
    }

    fn mtime(&self) -> Option<crate::stamp::FileStamp> {
        crate::stamp::FileStamp::of(&self.path)
    }

    fn refresh(&self) -> Result<()> {
        let stamp = self.mtime();
        let mut g = self.state.lock();
        if stamp.is_some() && g.stamp == stamp {
            return Ok(());
        }
        let mut file = match std::fs::read_to_string(&self.path) {
            Ok(s) => toml::from_str::<File>(&s).with_context(|| format!("reading {}", self.path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => File::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.path.display())),
        };
        if !file.folders_ordered {
            file.folders.sort_by_key(|d| d.name.to_lowercase());
        }
        for f in &mut file.favorites {
            if f.folder.as_ref().is_some_and(|id| !file.folders.iter().any(|d| d.id == *id)) {
                f.folder = None;
            }
        }
        g.stamp = stamp;
        g.items = file.favorites;
        g.folders = file.folders;
        Ok(())
    }

    pub fn list(&self) -> Vec<Favorite> {
        let _ = self.refresh();
        self.state.lock().items.clone()
    }

    pub fn add(&self, fav: Favorite) -> Result<Favorite> {
        let _ = self.refresh();
        let mut g = self.state.lock();
        if let Some(existing) = g.items.iter().find(|f| f.sql.trim() == fav.sql.trim()) {
            return Ok(existing.clone());
        }
        g.items.push(fav.clone());
        g.save(&self.path)?;
        Ok(fav)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        self.take(id).map(|_| ())
    }

    pub fn take(&self, id: &str) -> Result<Option<(usize, Favorite)>> {
        let _ = self.refresh();
        let mut g = self.state.lock();
        let Some(ix) = g.items.iter().position(|f| f.id == id) else { return Ok(None) };
        let fav = g.items.remove(ix);
        g.save(&self.path)?;
        Ok(Some((ix, fav)))
    }

    pub fn restore(&self, ix: usize, fav: Favorite) -> Result<bool> {
        let _ = self.refresh();
        let mut g = self.state.lock();
        let homeless = fav.folder.as_ref().is_some_and(|d| !g.folders.iter().any(|f| f.id == *d));
        if homeless || g.items.iter().any(|f| f.id == fav.id) {
            return Ok(false);
        }
        let ix = ix.min(g.items.len());
        g.items.insert(ix, fav);
        g.save(&self.path)?;
        Ok(true)
    }

    pub fn find_by_sql(&self, sql: &str) -> Option<Favorite> {
        let _ = self.refresh();
        self.state.lock().items.iter().find(|f| f.sql.trim() == sql.trim()).cloned()
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<Option<Favorite>> {
        let _ = self.refresh();
        let mut g = self.state.lock();
        let Some(f) = g.items.iter_mut().find(|f| f.id == id) else { return Ok(None) };
        f.name = name.to_string();
        let updated = f.clone();
        g.save(&self.path)?;
        Ok(Some(updated))
    }

    pub fn update_sql(&self, id: &str, sql: &str) -> Result<Option<Favorite>> {
        let _ = self.refresh();
        let mut g = self.state.lock();
        let Some(f) = g.items.iter_mut().find(|f| f.id == id) else { return Ok(None) };
        f.sql = sql.to_string();
        let updated = f.clone();
        g.save(&self.path)?;
        Ok(Some(updated))
    }

    pub fn folders(&self) -> Vec<Folder> {
        let _ = self.refresh();
        self.state.lock().folders.clone()
    }

    pub fn add_folder(&self, name: &str) -> Result<Folder> {
        let _ = self.refresh();
        let mut g = self.state.lock();
        let taken: Vec<&str> = g.folders.iter().map(|d| d.name.as_str()).collect();
        let base = if name.trim().is_empty() { "New folder" } else { name };
        let folder = Folder { id: uuid::Uuid::new_v4().simple().to_string(), name: unique_name(base, &taken) };
        g.folders.push(folder.clone());
        g.save(&self.path)?;
        Ok(folder)
    }

    pub fn rename_folder(&self, id: &str, name: &str) -> Result<Option<Folder>> {
        let _ = self.refresh();
        let mut g = self.state.lock();
        let Some(d) = g.folders.iter_mut().find(|d| d.id == id) else { return Ok(None) };
        d.name = name.to_string();
        let updated = d.clone();
        g.save(&self.path)?;
        Ok(Some(updated))
    }

    pub fn reorder_folders(&self, ids: &[String]) -> Result<()> {
        let _ = self.refresh();
        let mut g = self.state.lock();
        g.folders.sort_by_key(|d| ids.iter().position(|i| *i == d.id).unwrap_or(usize::MAX));
        g.save(&self.path)
    }

    pub fn remove_folder(&self, id: &str) -> Result<Option<RemovedFolder>> {
        let _ = self.refresh();
        let mut g = self.state.lock();
        let Some(index) = g.folders.iter().position(|d| d.id == id) else { return Ok(None) };
        let folder = g.folders.remove(index);
        let mut favorites = Vec::new();
        let mut i = 0;
        g.items.retain(|f| {
            let gone = f.folder.as_deref() == Some(id);
            if gone {
                favorites.push((i, f.clone()));
            }
            i += 1;
            !gone
        });
        g.save(&self.path)?;
        Ok(Some(RemovedFolder { index, folder, favorites }))
    }

    pub fn restore_folder(&self, r: &RemovedFolder) -> Result<bool> {
        let _ = self.refresh();
        let mut g = self.state.lock();
        if g.folders.iter().any(|d| d.id == r.folder.id) {
            return Ok(false);
        }
        let ix = r.index.min(g.folders.len());
        g.folders.insert(ix, r.folder.clone());
        for (i, f) in &r.favorites {
            if !g.items.iter().any(|x| x.id == f.id) {
                let i = (*i).min(g.items.len());
                g.items.insert(i, f.clone());
            }
        }
        g.save(&self.path)?;
        Ok(true)
    }

    /// Several favorites and folders changed in one write (an open project's sync).
    pub fn edit_all(&self, f: impl FnOnce(&mut Vec<Favorite>, &mut Vec<Folder>)) -> Result<()> {
        let _ = self.refresh();
        let mut g = self.state.lock();
        let Fresh { items, folders, .. } = &mut *g;
        f(items, folders);
        g.save(&self.path)
    }

    pub fn move_to_folder(&self, id: &str, folder: Option<&str>) -> Result<Option<Favorite>> {
        let _ = self.refresh();
        let mut g = self.state.lock();
        if let Some(d) = folder
            && !g.folders.iter().any(|f| f.id == d)
        {
            anyhow::bail!("no folder with id {d:?}");
        }
        let Some(f) = g.items.iter_mut().find(|f| f.id == id) else { return Ok(None) };
        f.folder = folder.map(str::to_string);
        let updated = f.clone();
        g.save(&self.path)?;
        Ok(Some(updated))
    }
}

pub fn unique_name(base: &str, taken: &[&str]) -> String {
    let base = base.trim();
    let base = if base.is_empty() { "query" } else { base };
    let used = |n: &str| taken.iter().any(|t| t.trim().eq_ignore_ascii_case(n));
    if !used(base) {
        return base.to_string();
    }
    (2..).map(|i| format!("{base} {i}")).find(|n| !used(n)).unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq)]
pub enum NameMatch {
    One(usize),
    Ambiguous(Vec<usize>),
    Nothing(Vec<usize>),
}

pub const MATCH_MIN: u32 = 45;
pub const MATCH_GAP: u32 = 8;
pub const MATCH_LIST: usize = 5;

pub fn fold_name(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.nfd().filter(|c| !unicode_normalization::char::is_combining_mark(*c)) {
        if c.is_alphanumeric() {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.extend(c.to_lowercase());
        } else {
            space = true;
        }
    }
    out
}

fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for j in 0..=b.len() {
        d[0][j] = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

fn word_like(a: &str, b: &str) -> f32 {
    if a == b {
        return 1.;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if short.chars().count() >= 3 && long.starts_with(short) {
        return 0.9;
    }
    let n = a.chars().count().max(b.chars().count()).max(1);
    (1. - distance(a, b) as f32 / n as f32).max(0.)
}

pub fn name_score(query: &str, name: &str) -> u32 {
    if query.is_empty() || name.is_empty() {
        return 0;
    }
    if query == name {
        return 100;
    }
    let (qw, nw): (Vec<&str>, Vec<&str>) = (query.split(' ').collect(), name.split(' ').collect());
    let (mut qs, mut ns) = (qw.clone(), nw.clone());
    qs.sort_unstable();
    ns.sort_unstable();
    if qs == ns {
        return 95;
    }
    if name.starts_with(query) {
        return 90;
    }
    if name.contains(query) {
        return 80;
    }
    let best = |w: &str, among: &[&str]| among.iter().map(|x| word_like(w, x)).fold(0f32, f32::max);
    let typed = qw.iter().map(|w| best(w, &nw)).sum::<f32>() / qw.len() as f32;
    let covered = nw.iter().filter(|w| best(w, &qw) >= 0.6).count() as f32 / nw.len() as f32;
    (70. * (0.75 * typed + 0.25 * covered)).round() as u32
}

pub fn match_name(names: &[&str], query: &str) -> NameMatch {
    let q = fold_name(query);
    let mut scored: Vec<(u32, usize)> = names.iter().enumerate().map(|(i, n)| (name_score(&q, &fold_name(n)), i)).collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let Some(&(best, ix)) = scored.first() else { return NameMatch::Nothing(Vec::new()) };
    if best < MATCH_MIN {
        return NameMatch::Nothing(scored.iter().filter(|s| s.0 > 0).take(MATCH_LIST).map(|s| s.1).collect());
    }
    let close: Vec<usize> = scored.iter().take_while(|s| s.0 + MATCH_GAP > best && s.0 >= MATCH_MIN).take(MATCH_LIST).map(|s| s.1).collect();
    if close.len() == 1 { NameMatch::One(ix) } else { NameMatch::Ambiguous(close) }
}

fn save(path: &Path, items: &[Favorite], folders: &[Folder]) -> Result<()> {
    let dir = path.parent().context("favorites path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let text = toml::to_string_pretty(&File { folders_ordered: !folders.is_empty(), folders: folders.to_vec(), favorites: items.to_vec() })?;
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
        let dirs = Dirs::at(dir.path().join("dataman"));
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
        let dirs = Dirs::at(dir.path().join("dataman"));
        let store = FavoritesStore::open(&dirs).unwrap();
        let f = store.add(Favorite::new("x", "select 1")).unwrap();
        store.remove(&f.id).unwrap();
        assert!(store.list().is_empty());
        assert!(FavoritesStore::open(&dirs).unwrap().list().is_empty());
    }

    #[test]
    fn rename_keeps_everything_else() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
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
    fn update_sql_keeps_the_name_and_where_it_runs() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        let store = FavoritesStore::open(&dirs).unwrap();
        let mut f = Favorite::new("daily", "select 1");
        f.connection_id = Some("c1".into());
        let f = store.add(f).unwrap();
        let u = store.update_sql(&f.id, "select 2").unwrap().unwrap();
        assert_eq!((u.id.as_str(), u.name.as_str(), u.sql.as_str(), u.connection_id.as_deref()), (f.id.as_str(), "daily", "select 2", Some("c1")));
        assert_eq!(FavoritesStore::open(&dirs).unwrap().list(), vec![u]);
        assert!(store.update_sql("nope", "x").unwrap().is_none());
    }

    #[test]
    fn unique_names() {
        assert_eq!(unique_name("orders", &[]), "orders");
        assert_eq!(unique_name("orders", &["Orders"]), "orders 2");
        assert_eq!(unique_name("orders", &["orders", "orders 2", "x"]), "orders 3");
        assert_eq!(unique_name("  ", &["query"]), "query 2");
    }

    #[test]
    fn names_typed_from_memory() {
        let names = ["Orders by day", "Orders by month", "Top customers", "Crème brûlée sales", "Slow queries (pg_stat_statements)", "orders"];
        let m = |q: &str| match_name(&names, q);
        assert_eq!(m("orders"), NameMatch::One(5));
        assert_eq!(m("TOP CUSTOMERS"), NameMatch::One(2));
        assert_eq!(m("creme brulee sales"), NameMatch::One(3));
        assert_eq!(m("Crème Brûlée Sales"), NameMatch::One(3));
        assert_eq!(m("customers top"), NameMatch::One(2));
        assert_eq!(m("slow queries pg stat statements"), NameMatch::One(4));
        assert_eq!(m("slow"), NameMatch::One(4));
        assert_eq!(m("brulee"), NameMatch::One(3));
        assert_eq!(m("ordrs by dya"), NameMatch::One(0));
        assert_eq!(m("orders by mnth"), NameMatch::One(1));
        assert_eq!(m("top custmers"), NameMatch::One(2));
        assert_eq!(m("cremme brule"), NameMatch::One(3));
        assert_eq!(m("customer top"), NameMatch::One(2));
        assert_eq!(m("orders by"), NameMatch::Ambiguous(vec![0, 1]));
        assert_eq!(m("order"), NameMatch::Ambiguous(vec![0, 1, 5]));
        assert!(matches!(m("inventory forecast"), NameMatch::Nothing(_)));
        assert_eq!(match_name(&[], "x"), NameMatch::Nothing(vec![]));
        assert_eq!(match_name(&["daily", "Daily"], "daily"), NameMatch::Ambiguous(vec![0, 1]));
        assert_eq!(fold_name("  Crème-Brûlée  (v2) "), "creme brulee v2");
    }

    #[test]
    fn a_file_without_folders_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        std::fs::create_dir_all(dirs.favorites_file().parent().unwrap()).unwrap();
        std::fs::write(
            dirs.favorites_file(),
            "[[favorite]]\nid = \"a1\"\nname = \"daily\"\nsql = \"select 1\"\nconnection_id = \"c1\"\n\n[[favorite]]\nid = \"b2\"\nname = \"weekly\"\nsql = \"select 2\"\n",
        )
        .unwrap();
        let store = FavoritesStore::open(&dirs).unwrap();
        let list = store.list();
        assert_eq!(list.iter().map(|f| (f.id.as_str(), f.folder.as_deref())).collect::<Vec<_>>(), vec![("a1", None), ("b2", None)]);
        assert!(store.folders().is_empty());
        store.rename("a1", "daily orders").unwrap();
        let text = std::fs::read_to_string(dirs.favorites_file()).unwrap();
        assert!(!text.contains("folder"), "{text}");
    }

    #[test]
    fn folders_round_trip_and_favorites_move_in_and_out() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        let store = FavoritesStore::open(&dirs).unwrap();
        let a = store.add(Favorite::new("a", "select 1")).unwrap();
        let b = store.add(Favorite::new("b", "select 2")).unwrap();
        let reports = store.add_folder("Reports").unwrap();
        let again = store.add_folder("reports").unwrap();
        assert_eq!(again.name, "reports 2", "a folder's name is made unique");
        assert_eq!(store.add_folder("  ").unwrap().name, "New folder");
        let moved = store.move_to_folder(&a.id, Some(&reports.id)).unwrap().unwrap();
        assert_eq!((moved.name.as_str(), moved.sql.as_str(), moved.folder.as_deref()), ("a", "select 1", Some(reports.id.as_str())));
        assert!(store.move_to_folder(&b.id, Some("nope")).is_err(), "no such folder");
        assert!(store.move_to_folder("nope", None).unwrap().is_none());

        let other = FavoritesStore::open(&dirs).unwrap();
        assert_eq!(other.folders().iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), vec!["Reports", "reports 2", "New folder"]);
        assert_eq!(other.list().iter().map(|f| f.folder.clone()).collect::<Vec<_>>(), vec![Some(reports.id.clone()), None]);

        let renamed = store.rename_folder(&reports.id, "Monthly").unwrap().unwrap();
        assert_eq!(renamed.name, "Monthly");
        assert!(store.rename_folder("nope", "x").unwrap().is_none());
        assert_eq!(store.list()[0].folder.as_deref(), Some(reports.id.as_str()), "a rename keeps its favorites");

        store.move_to_folder(&a.id, None).unwrap();
        assert_eq!(FavoritesStore::open(&dirs).unwrap().list()[0].folder, None);
    }

    #[test]
    fn removing_a_folder_removes_its_favorites_and_restores_them() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        let store = FavoritesStore::open(&dirs).unwrap();
        let first = store.add_folder("First").unwrap();
        let d = store.add_folder("Old").unwrap();
        let keep = store.add_folder("Keep").unwrap();
        for (i, folder) in [Some(&d.id), None, Some(&d.id), Some(&keep.id), None].into_iter().enumerate() {
            let f = store.add(Favorite::new(format!("f{i}"), format!("select {i}"))).unwrap();
            store.move_to_folder(&f.id, folder.map(String::as_str)).unwrap();
        }
        let before = (store.folders(), store.list());
        let gone = store.remove_folder(&d.id).unwrap().unwrap();
        assert_eq!((gone.index, gone.folder.name.as_str()), (1, "Old"));
        assert_eq!(gone.favorites.iter().map(|(i, f)| (*i, f.name.as_str())).collect::<Vec<_>>(), vec![(0, "f0"), (2, "f2")]);
        assert!(store.remove_folder(&d.id).unwrap().is_none(), "gone already");
        let again = FavoritesStore::open(&dirs).unwrap();
        assert_eq!(again.folders(), vec![first.clone(), keep.clone()]);
        assert_eq!(again.list().iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["f1", "f3", "f4"], "its favorites went with it, the others stay");

        assert!(store.restore_folder(&gone).unwrap());
        assert!(!store.restore_folder(&gone).unwrap(), "there already");
        let again = FavoritesStore::open(&dirs).unwrap();
        assert_eq!((again.folders(), again.list()), before);
    }

    #[test]
    fn a_removed_favorite_is_restored_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        let store = FavoritesStore::open(&dirs).unwrap();
        for i in 0..3 {
            store.add(Favorite::new(format!("f{i}"), format!("select {i}"))).unwrap();
        }
        let before = store.list();
        let (ix, f) = store.take(&before[1].id).unwrap().unwrap();
        assert_eq!((ix, f.name.as_str()), (1, "f1"));
        assert!(store.take(&f.id).unwrap().is_none());
        assert!(store.restore(ix, f.clone()).unwrap());
        assert!(!store.restore(ix, f.clone()).unwrap(), "no duplicate");
        assert_eq!(FavoritesStore::open(&dirs).unwrap().list(), before);
        let d = store.add_folder("D").unwrap();
        store.move_to_folder(&f.id, Some(&d.id)).unwrap();
        let (ix, f) = store.take(&f.id).unwrap().unwrap();
        store.remove_folder(&d.id).unwrap();
        assert!(!store.restore(ix, f).unwrap());
        assert_eq!(store.list().len(), 2);
    }

    #[test]
    fn a_dangling_folder_is_the_top_level() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        std::fs::create_dir_all(dirs.favorites_file().parent().unwrap()).unwrap();
        std::fs::write(
            dirs.favorites_file(),
            "[[folder]]\nid = \"d1\"\nname = \"Reports\"\n\n[[favorite]]\nid = \"a1\"\nname = \"in\"\nsql = \"select 1\"\nfolder = \"d1\"\n\n[[favorite]]\nid = \"b2\"\nname = \"lost\"\nsql = \"select 2\"\nfolder = \"gone\"\n",
        )
        .unwrap();
        let list = FavoritesStore::open(&dirs).unwrap().list();
        assert_eq!(list.iter().map(|f| f.folder.as_deref()).collect::<Vec<_>>(), vec![Some("d1"), None]);
    }

    fn folder_names(store: &FavoritesStore) -> Vec<String> {
        store.folders().into_iter().map(|d| d.name).collect()
    }

    #[test]
    fn folders_keep_the_users_order() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        let store = FavoritesStore::open(&dirs).unwrap();
        let (c, a, b) = (store.add_folder("Charts").unwrap(), store.add_folder("Archive").unwrap(), store.add_folder("Billing").unwrap());
        assert_eq!(folder_names(&store), ["Charts", "Archive", "Billing"], "made last, listed last");
        store.reorder_folders(&[b.id.clone(), a.id.clone()]).unwrap();
        assert_eq!(folder_names(&store), ["Billing", "Archive", "Charts"]);
        let other = FavoritesStore::open(&dirs).unwrap();
        assert_eq!(folder_names(&other), ["Billing", "Archive", "Charts"], "the file keeps the order");
        store.rename_folder(&a.id, "Zzz").unwrap();
        store.add_folder("Aaa").unwrap();
        assert_eq!(folder_names(&other), ["Billing", "Zzz", "Charts", "Aaa"]);
        store.remove_folder(&b.id).unwrap();
        store.reorder_folders(&[c.id.clone()]).unwrap();
        assert_eq!(folder_names(&FavoritesStore::open(&dirs).unwrap()), ["Charts", "Zzz", "Aaa"]);
    }

    #[test]
    fn an_older_file_keeps_its_folders_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        std::fs::create_dir_all(dirs.favorites_file().parent().unwrap()).unwrap();
        let folders = "[[folder]]\nid = \"z\"\nname = \"Zeta\"\n\n[[folder]]\nid = \"a\"\nname = \"alpha\"\n\n[[folder]]\nid = \"m\"\nname = \"Mid\"\n";
        std::fs::write(dirs.favorites_file(), format!("{folders}\n[[favorite]]\nid = \"f1\"\nname = \"in\"\nsql = \"select 1\"\nfolder = \"z\"\n")).unwrap();
        let store = FavoritesStore::open(&dirs).unwrap();
        assert_eq!(folder_names(&store), ["alpha", "Mid", "Zeta"]);
        assert_eq!(store.list()[0].folder.as_deref(), Some("z"));
        store.add(Favorite::new("x", "select 2")).unwrap();
        let text = std::fs::read_to_string(dirs.favorites_file()).unwrap();
        assert!(text.contains("folders_ordered = true"), "{text}");
        store.reorder_folders(&["z".into()]).unwrap();
        assert_eq!(folder_names(&FavoritesStore::open(&dirs).unwrap()), ["Zeta", "alpha", "Mid"], "reordered, not by name again");

        std::fs::write(dirs.favorites_file(), format!("folders_ordered = true\n\n{folders}")).unwrap();
        assert_eq!(folder_names(&FavoritesStore::open(&dirs).unwrap()), ["Zeta", "alpha", "Mid"]);
    }

    #[test]
    fn sees_another_process_writing_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        let store = FavoritesStore::open(&dirs).unwrap();
        assert!(store.list().is_empty());
        let other = FavoritesStore::open(&dirs).unwrap();
        other.add(Favorite::new("x", "select 1")).unwrap();
        assert_eq!(store.list().len(), 1, "re-read after the file's mtime changed");
    }
}
