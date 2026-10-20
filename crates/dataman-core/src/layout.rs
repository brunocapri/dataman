use std::hash::{Hash, Hasher};

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dock {
    Left,
    Right,
}

impl Dock {
    pub const ALL: [Dock; 2] = [Dock::Left, Dock::Right];

    pub fn other(self) -> Dock {
        match self {
            Dock::Left => Dock::Right,
            Dock::Right => Dock::Left,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Dock::Left => "left",
            Dock::Right => "right",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelId {
    Explorer,
    // Files from 0.3 call it "history".
    #[serde(alias = "history")]
    Side,
}

impl PanelId {
    pub const ALL: [PanelId; 2] = [PanelId::Explorer, PanelId::Side];

    pub fn home(self) -> Dock {
        match self {
            PanelId::Explorer => Dock::Left,
            PanelId::Side => Dock::Right,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PanelId::Explorer => "Explorer",
            PanelId::Side => "Side panel",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavView {
    #[default]
    Explorer,
    Favorites,
    History,
    /// The Agents area: its own icon at the foot of the rail, not one of `ALL`.
    Agents,
}

impl NavView {
    pub const ALL: [NavView; 3] = [NavView::Explorer, NavView::Favorites, NavView::History];

    pub fn label(self) -> &'static str {
        match self {
            NavView::Explorer => "Explorer",
            NavView::Favorites => "Favorites",
            NavView::History => "History",
            NavView::Agents => "Agents",
        }
    }
}

pub const EVEN: f32 = 0.5;

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

    pub fn share(&self, i: usize) -> f32 {
        match self.panels.len() {
            0 | 1 => 1.,
            _ if i == 0 => self.split,
            n => (1. - self.split) / (n - 1) as f32,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Place {
    pub dock: Dock,
    pub col: usize,
    pub row: Option<usize>,
}

impl Place {
    pub fn column(dock: Dock, col: usize) -> Place {
        Place { dock, col, row: None }
    }

    pub fn stack(dock: Dock, col: usize, row: usize) -> Place {
        Place { dock, col, row: Some(row) }
    }
}

#[derive(Clone, Debug, PartialEq, Hash, Serialize, Deserialize)]
pub struct PanelLayout {
    #[serde(default, deserialize_with = "dock_columns")]
    pub left: Vec<Column>,
    #[serde(default, deserialize_with = "dock_columns")]
    pub right: Vec<Column>,
}

// Older files wrote a dock as its panels (one column): both forms must still read.
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
    fn default() -> Self {
        PanelLayout::explorer_at(Dock::Left)
    }
}

impl PanelLayout {
    pub fn explorer_at(d: Dock) -> PanelLayout {
        let (e, s) = (vec![Column::of(vec![PanelId::Explorer])], vec![Column::of(vec![PanelId::Side])]);
        match d {
            Dock::Left => PanelLayout { left: e, right: s },
            Dock::Right => PanelLayout { left: s, right: e },
        }
    }

    pub fn dock(&self, d: Dock) -> &[Column] {
        match d {
            Dock::Left => &self.left,
            Dock::Right => &self.right,
        }
    }

    pub fn panels(&self, d: Dock) -> impl Iterator<Item = PanelId> + '_ {
        self.dock(d).iter().flat_map(|c| c.panels.iter().copied())
    }

    pub fn explorer_dock(&self) -> Dock {
        self.locate(PanelId::Explorer).map_or(Dock::Left, |(d, ..)| d)
    }

    pub fn place_of(&self, p: PanelId) -> Place {
        match self.locate(p) {
            Some((d, ci, ri)) => Place { dock: d, col: ci, row: (self.dock(d)[ci].panels.len() > 1).then_some(ri) },
            None => Place::column(p.home(), 0),
        }
    }

    pub fn locate(&self, p: PanelId) -> Option<(Dock, usize, usize)> {
        Dock::ALL.into_iter().find_map(|d| {
            self.dock(d).iter().enumerate().find_map(|(ci, c)| c.panels.iter().position(|x| *x == p).map(|ri| (d, ci, ri)))
        })
    }

    pub fn moved(&self, p: PanelId, to: Place) -> PanelLayout {
        PanelLayout::explorer_at(if p == PanelId::Explorer { to.dock } else { to.dock.other() })
    }

    pub fn normalized(self) -> PanelLayout {
        PanelLayout::explorer_at(self.explorer_dock())
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
    fn two_arrangements() {
        let l = PanelLayout::default();
        assert_eq!(l.place_of(Explorer), Place::column(Dock::Left, 0));
        assert_eq!(l.place_of(Side), Place::column(Dock::Right, 0));
        let r = l.moved(Explorer, Place::stack(Dock::Right, 0, 0));
        assert_eq!((panels(&r.left), panels(&r.right)), (vec![vec![Side]], vec![vec![Explorer]]));
        assert_eq!(r.explorer_dock(), Dock::Right);
        assert!(r.moved(Side, Place::column(Dock::Right, 3)).is_default());
        assert_eq!(l.moved(Explorer, Place::column(Dock::Left, 1)), l);
    }

    #[test]
    fn a_damaged_arrangement_reads_sanely() {
        let twice = PanelLayout { left: vec![Column::of(vec![Explorer, Explorer]), Column::of(vec![])], right: vec![Column { panels: vec![Explorer], split: f32::NAN }] };
        assert!(twice.normalized().is_default());
        let none = PanelLayout { left: vec![], right: vec![] }.normalized();
        assert!(none.is_default());
        let json = serde_json::to_string(&PanelLayout::default()).unwrap();
        assert_eq!(json, r#"{"left":[{"panels":["explorer"],"split":0.5}],"right":[{"panels":["side"],"split":0.5}]}"#);
        let back: PanelLayout = serde_json::from_str(&json).unwrap();
        assert!(back.is_default());
    }

    #[test]
    fn older_files_read_as_their_explorer_dock() {
        let l: PanelLayout = serde_json::from_str(r#"{"left":["explorer","side"],"right":[],"bottom":[]}"#).unwrap();
        assert!(l.normalized().is_default());
        let l: PanelLayout = serde_json::from_str(r#"{"left":[],"right":["side"],"bottom":["explorer"],"extra":1}"#).unwrap();
        assert!(l.normalized().is_default());
        let l: PanelLayout = serde_json::from_str(r#"{"right":[{"panels":["history","explorer"]}]}"#).unwrap();
        assert_eq!(l.normalized(), PanelLayout::explorer_at(Dock::Right));
        let l: PanelLayout = serde_json::from_str(r#"{"left":[{"panels":["history"]}],"right":[{"panels":["explorer"]}]}"#).unwrap();
        assert_eq!(panels(&l.normalized().left), [vec![Side]]);
    }
}
