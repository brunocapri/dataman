//! Where the window's movable panels sit: a dock at each side of the work surface (the editor
//! and its results), each holding columns of panels side by side. The left dock runs the
//! window's full height on the glass; the right one sits inside the work column. Panels sharing
//! a column stack top to bottom, the top one taking the column's `split` of its height.
//!
//! Kept in the session (`session.json`), so the arrangement survives a relaunch. A file that
//! names a panel twice, or not at all, reads as a sane arrangement ([`PanelLayout::normalized`]):
//! it never keeps the window from opening. Older files read as the arrangement they described:
//! a dock's list of panels is one column, and a panel in the bottom dock (gone) goes home.

use std::hash::{Hash, Hasher};

use serde::{Deserialize, Deserializer, Serialize};

/// A dock beside the work surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dock {
    Left,
    Right,
}

impl Dock {
    pub const ALL: [Dock; 2] = [Dock::Left, Dock::Right];

    pub fn name(self) -> &'static str {
        match self {
            Dock::Left => "left",
            Dock::Right => "right",
        }
    }
}

/// A panel that can be moved between the docks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelId {
    /// The connections and schema tree.
    Explorer,
    /// The history and favorites, and what opens in their place: a server panel (LAR), a
    /// feature's sheet (the grid-edit review, the MCP log…). Older files call it "side".
    #[serde(alias = "side")]
    History,
}

impl PanelId {
    pub const ALL: [PanelId; 2] = [PanelId::Explorer, PanelId::History];

    /// Where it goes when the arrangement does not say (and after a reset).
    pub fn home(self) -> Dock {
        match self {
            PanelId::Explorer => Dock::Left,
            PanelId::History => Dock::Right,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PanelId::Explorer => "Explorer",
            PanelId::History => "History",
        }
    }
}

/// A column's even split, and the least share of its height a stacked panel keeps.
pub const EVEN: f32 = 0.5;
pub const SPLIT_MIN: f32 = 0.1;

/// Panels stacked top to bottom in a dock; `split` is the first one's share of the height
/// (the others share the rest evenly).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub panels: Vec<PanelId>,
    #[serde(default = "even")]
    pub split: f32,
}

fn even() -> f32 {
    EVEN
}

impl Hash for Column {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.panels.hash(h);
        self.split.to_bits().hash(h);
    }
}

impl Column {
    pub fn of(panels: Vec<PanelId>) -> Column {
        Column { panels, split: EVEN }
    }

    /// The share of the column's height its `i`th panel asks for (all of it when alone).
    pub fn share(&self, i: usize) -> f32 {
        match self.panels.len() {
            0 | 1 => 1.,
            _ if i == 0 => self.split,
            n => (1. - self.split) / (n - 1) as f32,
        }
    }
}

/// A position in a dock: a column of its own at the gap `col` among the dock's columns
/// (`row: None`), or the gap `row` among column `col`'s panels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Place {
    pub dock: Dock,
    pub col: usize,
    pub row: Option<usize>,
}

impl Place {
    /// A column of its own.
    pub fn column(dock: Dock, col: usize) -> Place {
        Place { dock, col, row: None }
    }

    /// Stacked in a column.
    pub fn stack(dock: Dock, col: usize, row: usize) -> Place {
        Place { dock, col, row: Some(row) }
    }
}

/// The columns of each dock, left to right.
#[derive(Clone, Debug, PartialEq, Hash, Serialize, Deserialize)]
pub struct PanelLayout {
    #[serde(default, deserialize_with = "dock_columns")]
    pub left: Vec<Column>,
    #[serde(default, deserialize_with = "dock_columns")]
    pub right: Vec<Column>,
}

/// A dock as written: its columns, or (older files) its panels, one column.
#[derive(Deserialize)]
#[serde(untagged)]
enum DockRepr {
    Columns(Vec<Column>),
    Panels(Vec<PanelId>),
}

fn dock_columns<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Column>, D::Error> {
    Ok(match DockRepr::deserialize(d)? {
        DockRepr::Columns(c) => c,
        DockRepr::Panels(p) if p.is_empty() => Vec::new(),
        DockRepr::Panels(p) => vec![Column::of(p)],
    })
}

impl Default for PanelLayout {
    /// The explorer on the left, the history on the right, each a column of its own.
    fn default() -> Self {
        PanelLayout { left: vec![Column::of(vec![PanelId::Explorer])], right: vec![Column::of(vec![PanelId::History])] }
    }
}

impl PanelLayout {
    pub fn dock(&self, d: Dock) -> &[Column] {
        match d {
            Dock::Left => &self.left,
            Dock::Right => &self.right,
        }
    }

    fn dock_mut(&mut self, d: Dock) -> &mut Vec<Column> {
        match d {
            Dock::Left => &mut self.left,
            Dock::Right => &mut self.right,
        }
    }

    /// The panels of dock `d`, column by column.
    pub fn panels(&self, d: Dock) -> impl Iterator<Item = PanelId> + '_ {
        self.dock(d).iter().flat_map(|c| c.panels.iter().copied())
    }

    /// Where `p` is (a normalized arrangement has every panel exactly once): alone in its
    /// column it is a column of its own (`row: None`).
    pub fn place_of(&self, p: PanelId) -> Place {
        match self.locate(p) {
            Some((d, ci, ri)) => Place { dock: d, col: ci, row: (self.dock(d)[ci].panels.len() > 1).then_some(ri) },
            None => Place::column(p.home(), 0),
        }
    }

    /// The dock, column and row `p` is at.
    pub fn locate(&self, p: PanelId) -> Option<(Dock, usize, usize)> {
        Dock::ALL.into_iter().find_map(|d| {
            self.dock(d).iter().enumerate().find_map(|(ci, c)| c.panels.iter().position(|x| *x == p).map(|ri| (d, ci, ri)))
        })
    }

    /// Whether `p` shares its dock with another panel.
    pub fn shares_dock(&self, p: PanelId) -> bool {
        let d = self.place_of(p).dock;
        self.panels(d).any(|x| x != p)
    }

    /// The arrangement without `p`: an emptied column goes, one left alone is even again.
    pub fn without(&self, p: PanelId) -> PanelLayout {
        let mut l = self.clone();
        for d in Dock::ALL {
            let cols = l.dock_mut(d);
            for c in cols.iter_mut() {
                let n = c.panels.len();
                c.panels.retain(|x| *x != p);
                if c.panels.len() != n && c.panels.len() < 2 {
                    c.split = EVEN;
                }
            }
            cols.retain(|c| !c.panels.is_empty());
        }
        l
    }

    /// The arrangement with `p` taken out and put at `to` (its gaps counted without `p`,
    /// clamped to the dock's end). A column `p` joins splits evenly; one it stays in keeps its
    /// split.
    pub fn moved(&self, p: PanelId, to: Place) -> PanelLayout {
        let mut l = self.without(p);
        let cols = l.dock_mut(to.dock);
        match to.row.map(|r| (r, to.col)).filter(|(_, c)| *c < cols.len()) {
            Some((r, c)) => {
                let col = &mut cols[c];
                col.panels.insert(r.min(col.panels.len()), p);
                col.split = EVEN;
            }
            None => cols.insert(to.col.min(cols.len()), Column::of(vec![p])),
        }
        if l.same_places(self) { self.clone() } else { l }
    }

    /// The same panels in the same columns (whatever the splits).
    pub fn same_places(&self, other: &PanelLayout) -> bool {
        Dock::ALL.into_iter().all(|d| {
            let (a, b) = (self.dock(d), other.dock(d));
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.panels == y.panels)
        })
    }

    /// Column `col` of dock `d` split at `split` (kept within [`SPLIT_MIN`]).
    pub fn with_split(&self, d: Dock, col: usize, split: f32) -> PanelLayout {
        let mut l = self.clone();
        if let Some(c) = l.dock_mut(d).get_mut(col) {
            c.split = if split.is_finite() { split.clamp(SPLIT_MIN, 1. - SPLIT_MIN) } else { EVEN };
        }
        l
    }

    /// Every panel exactly once, every column with a panel and a sane split: a repeat is
    /// dropped (the first stays), a missing one goes back to its home dock, a column of its own
    /// at the window's edge.
    pub fn normalized(mut self) -> PanelLayout {
        let mut seen = Vec::new();
        for d in Dock::ALL {
            let cols = self.dock_mut(d);
            for c in cols.iter_mut() {
                c.panels.retain(|p| {
                    let first = !seen.contains(p);
                    seen.push(*p);
                    first
                });
                c.split = if c.panels.len() > 1 && c.split.is_finite() { c.split.clamp(SPLIT_MIN, 1. - SPLIT_MIN) } else { EVEN };
            }
            cols.retain(|c| !c.panels.is_empty());
        }
        for p in PanelId::ALL {
            if !seen.contains(&p) {
                let cols = self.dock_mut(p.home());
                let at = if p.home() == Dock::Left { 0 } else { cols.len() };
                cols.insert(at, Column::of(vec![p]));
            }
        }
        self
    }

    pub fn is_default(&self) -> bool {
        *self == PanelLayout::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PanelId::*;

    fn panels(cols: &[Column]) -> Vec<Vec<PanelId>> {
        cols.iter().map(|c| c.panels.clone()).collect()
    }

    #[test]
    fn moves_between_docks_columns_and_stacks() {
        let l = PanelLayout::default();
        assert_eq!(l.place_of(Explorer), Place::column(Dock::Left, 0));
        assert_eq!(l.place_of(History), Place::column(Dock::Right, 0));
        // The explorer to the right, stacked above the history: the left dock empties.
        let r = l.moved(Explorer, Place::stack(Dock::Right, 0, 0));
        assert!(r.left.is_empty());
        assert_eq!(panels(&r.right), [vec![Explorer, History]]);
        assert_eq!(r.place_of(History), Place::stack(Dock::Right, 0, 1));
        assert!(r.shares_dock(History) && !l.shares_dock(History));
        // Within the column: below the history (the gap counted without the explorer).
        assert_eq!(panels(&r.moved(Explorer, Place::stack(Dock::Right, 0, 1)).right), [vec![History, Explorer]]);
        // Out of the stack into a column of its own, beside it (right of it).
        let cols = r.moved(Explorer, Place::column(Dock::Right, 1));
        assert_eq!(panels(&cols.right), [vec![History], vec![Explorer]]);
        // The history beside the explorer, left of it, in the left dock.
        let beside = l.moved(History, Place::column(Dock::Left, 0));
        assert_eq!(panels(&beside.left), [vec![History], vec![Explorer]]);
        assert!(beside.right.is_empty());
        // Past the end is the end; a stack in a column that is not there is a column.
        assert_eq!(panels(&l.moved(History, Place::column(Dock::Left, 9)).left), [vec![Explorer], vec![History]]);
        assert_eq!(panels(&l.moved(History, Place::stack(Dock::Left, 4, 0)).left), [vec![Explorer], vec![History]]);
        // Back where it was: the same arrangement, its split kept.
        assert_eq!(l.moved(Explorer, Place::column(Dock::Left, 0)), l);
        let tall = r.with_split(Dock::Right, 0, 0.7);
        assert_eq!(tall.moved(History, tall.place_of(History)), tall);
        assert_eq!(tall.right[0].share(0), 0.7);
        assert!((tall.right[0].share(1) - 0.3).abs() < 1e-6);
        // Leaving a stack: the one left is a column of its own, even again.
        assert_eq!(tall.without(History).right, [Column::of(vec![Explorer])]);
        assert_eq!(tall.with_split(Dock::Right, 0, 0.99).right[0].split, 1. - SPLIT_MIN);
    }

    #[test]
    fn a_damaged_arrangement_reads_sanely() {
        let twice = PanelLayout { left: vec![Column::of(vec![Explorer, Explorer]), Column::of(vec![])], right: vec![Column { panels: vec![Explorer], split: f32::NAN }] };
        let n = twice.normalized();
        assert_eq!(n, PanelLayout { left: vec![Column::of(vec![Explorer])], right: vec![Column::of(vec![History])] });
        let none = PanelLayout { left: vec![], right: vec![] }.normalized();
        assert!(none.is_default());
        let json = serde_json::to_string(&PanelLayout::default()).unwrap();
        assert_eq!(json, r#"{"left":[{"panels":["explorer"],"split":0.5}],"right":[{"panels":["history"],"split":0.5}]}"#);
        let back: PanelLayout = serde_json::from_str(&json).unwrap();
        assert!(back.is_default());
    }

    /// Files from before columns: a dock's list is one column, "side" is the history, and a
    /// panel in the bottom dock (gone) goes back home.
    #[test]
    fn older_files_read_as_they_were() {
        let l: PanelLayout = serde_json::from_str(r#"{"left":["explorer","side"],"right":[],"bottom":[]}"#).unwrap();
        assert_eq!(panels(&l.normalized().left), [vec![Explorer, History]]);
        let l: PanelLayout = serde_json::from_str(r#"{"left":[],"right":["side"],"bottom":["explorer"],"extra":1}"#).unwrap();
        assert!(l.normalized().is_default());
        let l: PanelLayout = serde_json::from_str(r#"{"left":["explorer"],"bottom":["side"]}"#).unwrap();
        assert!(l.normalized().is_default());
        // A column without a split splits evenly.
        let l: PanelLayout = serde_json::from_str(r#"{"right":[{"panels":["history","explorer"]}]}"#).unwrap();
        assert_eq!(l.normalized().right, [Column::of(vec![History, Explorer])]);
    }
}
