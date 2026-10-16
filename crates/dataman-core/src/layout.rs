//! Where the window's movable panels sit: three docks around the work surface (the editor and
//! its results), each holding an ordered list of panels. The left dock runs the window's full
//! height on the glass; the right and bottom docks sit inside the work column, the bottom one
//! under the work surface only (between the left and right docks). Panels sharing a dock split
//! it: stacked top to bottom in the left and right docks, side by side in the bottom one.
//!
//! Kept in the session (`session.json`), so the arrangement survives a relaunch. A file that
//! names a panel twice, or not at all, reads as a sane arrangement ([`PanelLayout::normalized`]):
//! it never keeps the window from opening.

use serde::{Deserialize, Serialize};

/// A dock around the work surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dock {
    Left,
    Right,
    Bottom,
}

impl Dock {
    pub const ALL: [Dock; 3] = [Dock::Left, Dock::Right, Dock::Bottom];

    /// Its panels stack top to bottom (left, right) rather than side by side (bottom).
    pub fn stacks(self) -> bool {
        self != Dock::Bottom
    }

    pub fn name(self) -> &'static str {
        match self {
            Dock::Left => "left",
            Dock::Right => "right",
            Dock::Bottom => "bottom",
        }
    }
}

/// A panel that can be moved between the docks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelId {
    /// The connections and schema tree.
    Explorer,
    /// The side panel: the history, the cell viewer, a server panel (LAR), a feature's sheet.
    Side,
}

impl PanelId {
    pub const ALL: [PanelId; 2] = [PanelId::Explorer, PanelId::Side];

    /// Where it goes when the arrangement does not say (and after a reset).
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

/// A position in a dock: the gap `index` among its panels (0: first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Place {
    pub dock: Dock,
    pub index: usize,
}

/// The panels of each dock, in order.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PanelLayout {
    #[serde(default)]
    pub left: Vec<PanelId>,
    #[serde(default)]
    pub right: Vec<PanelId>,
    #[serde(default)]
    pub bottom: Vec<PanelId>,
}

impl Default for PanelLayout {
    /// The explorer on the left, the side panel on the right, nothing at the bottom.
    fn default() -> Self {
        let mut l = PanelLayout { left: vec![], right: vec![], bottom: vec![] };
        for p in PanelId::ALL {
            l.dock_mut(p.home()).push(p);
        }
        l
    }
}

impl PanelLayout {
    pub fn dock(&self, d: Dock) -> &[PanelId] {
        match d {
            Dock::Left => &self.left,
            Dock::Right => &self.right,
            Dock::Bottom => &self.bottom,
        }
    }

    fn dock_mut(&mut self, d: Dock) -> &mut Vec<PanelId> {
        match d {
            Dock::Left => &mut self.left,
            Dock::Right => &mut self.right,
            Dock::Bottom => &mut self.bottom,
        }
    }

    /// Where `p` is (a normalized arrangement has every panel exactly once).
    pub fn place_of(&self, p: PanelId) -> Place {
        Dock::ALL
            .into_iter()
            .find_map(|d| self.dock(d).iter().position(|x| *x == p).map(|index| Place { dock: d, index }))
            .unwrap_or(Place { dock: p.home(), index: 0 })
    }

    /// The arrangement with `p` taken out and put at `to` (its gap counted without `p`,
    /// clamped to the dock's end).
    pub fn moved(&self, p: PanelId, to: Place) -> PanelLayout {
        let mut l = self.clone();
        for d in Dock::ALL {
            l.dock_mut(d).retain(|x| *x != p);
        }
        let list = l.dock_mut(to.dock);
        list.insert(to.index.min(list.len()), p);
        l
    }

    /// Every panel exactly once: a repeat is dropped (the first stays), a missing one goes back
    /// to its home dock.
    pub fn normalized(mut self) -> PanelLayout {
        let mut seen = Vec::new();
        for d in Dock::ALL {
            self.dock_mut(d).retain(|p| {
                let first = !seen.contains(p);
                seen.push(*p);
                first
            });
        }
        for p in PanelId::ALL {
            if !seen.contains(&p) {
                self.dock_mut(p.home()).push(p);
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

    #[test]
    fn moves_between_docks_and_within_one() {
        let l = PanelLayout::default();
        assert_eq!(l.place_of(Explorer), Place { dock: Dock::Left, index: 0 });
        assert_eq!(l.place_of(Side), Place { dock: Dock::Right, index: 0 });
        // The explorer to the right, above the side panel: the left dock empties.
        let r = l.moved(Explorer, Place { dock: Dock::Right, index: 0 });
        assert_eq!((r.left.as_slice(), r.right.as_slice()), (&[][..], &[Explorer, Side][..]));
        // Within the dock: below the side panel (the gap is counted without the explorer).
        let r2 = r.moved(Explorer, Place { dock: Dock::Right, index: 1 });
        assert_eq!(r2.right, [Side, Explorer]);
        // Past the end is the end.
        assert_eq!(l.moved(Side, Place { dock: Dock::Left, index: 9 }).left, [Explorer, Side]);
        assert_eq!(l.moved(Side, Place { dock: Dock::Bottom, index: 0 }).bottom, [Side]);
        // Back where it was: the same arrangement.
        assert_eq!(l.moved(Explorer, Place { dock: Dock::Left, index: 0 }), l);
    }

    #[test]
    fn a_damaged_arrangement_reads_sanely() {
        let twice = PanelLayout { left: vec![Explorer, Explorer], right: vec![], bottom: vec![Explorer] };
        let n = twice.normalized();
        assert_eq!(n, PanelLayout { left: vec![Explorer], right: vec![Side], bottom: vec![] });
        let none = PanelLayout { left: vec![], right: vec![], bottom: vec![] }.normalized();
        assert!(none.is_default());
        // Unknown keys and a missing dock in the file.
        let l: PanelLayout = serde_json::from_str(r#"{"bottom":["explorer"],"extra":1}"#).unwrap();
        assert_eq!(l.normalized(), PanelLayout { left: vec![], right: vec![Side], bottom: vec![Explorer] });
        let json = serde_json::to_string(&PanelLayout::default()).unwrap();
        assert_eq!(json, r#"{"left":["explorer"],"right":["side"],"bottom":[]}"#);
    }
}
