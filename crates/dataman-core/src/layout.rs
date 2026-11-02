use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dock {
    Left,
    Right,
}

impl Dock {
    pub const ALL: [Dock; 2] = [Dock::Left, Dock::Right];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PanelId {
    Explorer,
    Side,
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

#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub panels: Vec<PanelId>,
    pub split: f32,
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

/// The one arrangement: the sidebar on the left, the side panel on the right.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelLayout {
    pub left: Vec<Column>,
    pub right: Vec<Column>,
}

impl Default for PanelLayout {
    fn default() -> Self {
        PanelLayout { left: vec![Column::of(vec![PanelId::Explorer])], right: vec![Column::of(vec![PanelId::Side])] }
    }
}

impl PanelLayout {
    pub fn dock(&self, d: Dock) -> &[Column] {
        match d {
            Dock::Left => &self.left,
            Dock::Right => &self.right,
        }
    }
}
