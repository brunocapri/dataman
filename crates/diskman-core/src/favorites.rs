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

    /// Replaces a favorite's statement (its name, connection and database stay); `None` when
    /// `id` is not found.
    pub fn update_sql(&self, id: &str, sql: &str) -> Result<Option<Favorite>> {
        let _ = self.refresh();
        let mut g = self.state.lock();
        let Some(f) = g.items.iter_mut().find(|f| f.id == id) else { return Ok(None) };
        f.sql = sql.to_string();
        let updated = f.clone();
        save(&self.path, &g.items)?;
        g.stamp = self.mtime();
        Ok(Some(updated))
    }
}

/// `base`, or `base 2`, `base 3`… : the first one no name in `taken` has (case aside).
pub fn unique_name(base: &str, taken: &[&str]) -> String {
    let base = base.trim();
    let base = if base.is_empty() { "query" } else { base };
    let used = |n: &str| taken.iter().any(|t| t.trim().eq_ignore_ascii_case(n));
    if !used(base) {
        return base.to_string();
    }
    (2..).map(|i| format!("{base} {i}")).find(|n| !used(n)).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Finding a favorite by a name typed from memory (`run_favorite`)

/// What a typed name finds among the favorites' names (indices into the names given).
#[derive(Clone, Debug, PartialEq)]
pub enum NameMatch {
    /// One clear best match.
    One(usize),
    /// Several close matches, best first: nobody can tell which one was meant.
    Ambiguous(Vec<usize>),
    /// Nothing close enough; the nearest names, best first (possibly none).
    Nothing(Vec<usize>),
}

/// The lowest score a name needs to be a match at all.
pub const MATCH_MIN: u32 = 45;
/// How far ahead of the next one the best match must be to be the one meant.
pub const MATCH_GAP: u32 = 8;
/// Candidates listed when the name is ambiguous or matches nothing.
pub const MATCH_LIST: usize = 5;

/// Lower case, accents dropped (`Crème` → `creme`), anything but letters and digits a space,
/// spaces collapsed.
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

/// Edit distance with adjacent transpositions (`dya` is one edit from `day`).
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

/// How alike two words are, 0 to 1: 1 when equal, 0.9 when one starts the other (three
/// letters at least), else what the edit distance leaves of the longer one.
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

/// How well `query` names `name`, both folded ([`fold_name`]), 0 to 100: 100 the same name,
/// 95 the same words in another order, 90 a prefix of it, 80 inside it; otherwise up to 70 by
/// its words: how alike each typed word is to the closest word of the name (three quarters)
/// and how many of the name's words were typed (a quarter).
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

/// Finds the name `query` means among `names`: the exact name (case and accents aside), the
/// same words in any order, a prefix, a part, or the closest by its words despite typos. One
/// match when the best scores at least [`MATCH_MIN`] and [`MATCH_GAP`] ahead of the next
/// (two favorites of the same name are ambiguous); else the closest names to choose from.
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
    fn update_sql_keeps_the_name_and_where_it_runs() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("diskman"));
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

    /// `run_favorite`'s matcher: a name typed from memory finds its favorite; an unclear one
    /// finds the candidates.
    #[test]
    fn names_typed_from_memory() {
        let names = ["Orders by day", "Orders by month", "Top customers", "Crème brûlée sales", "Slow queries (pg_stat_statements)", "orders"];
        let m = |q: &str| match_name(&names, q);
        // Exact, case and accents aside.
        assert_eq!(m("orders"), NameMatch::One(5));
        assert_eq!(m("TOP CUSTOMERS"), NameMatch::One(2));
        assert_eq!(m("creme brulee sales"), NameMatch::One(3));
        assert_eq!(m("Crème Brûlée Sales"), NameMatch::One(3));
        // Word order, punctuation.
        assert_eq!(m("customers top"), NameMatch::One(2));
        assert_eq!(m("slow queries pg stat statements"), NameMatch::One(4));
        // A prefix or a part that only one has.
        assert_eq!(m("slow"), NameMatch::One(4));
        assert_eq!(m("brulee"), NameMatch::One(3));
        // Typos.
        assert_eq!(m("ordrs by dya"), NameMatch::One(0));
        assert_eq!(m("orders by mnth"), NameMatch::One(1));
        assert_eq!(m("top custmers"), NameMatch::One(2));
        assert_eq!(m("cremme brule"), NameMatch::One(3));
        assert_eq!(m("customer top"), NameMatch::One(2));
        // Two equally close: ambiguous, the close ones listed, best first.
        assert_eq!(m("orders by"), NameMatch::Ambiguous(vec![0, 1]));
        assert_eq!(m("order"), NameMatch::Ambiguous(vec![0, 1, 5]));
        // Nothing near: the nearest names, but no match.
        assert!(matches!(m("inventory forecast"), NameMatch::Nothing(_)));
        assert_eq!(match_name(&[], "x"), NameMatch::Nothing(vec![]));
        // Two favorites with the same name cannot be told apart.
        assert_eq!(match_name(&["daily", "Daily"], "daily"), NameMatch::Ambiguous(vec![0, 1]));
        assert_eq!(fold_name("  Crème-Brûlée  (v2) "), "creme brulee v2");
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
