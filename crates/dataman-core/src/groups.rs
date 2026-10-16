//! Two editor groups side by side (VS Code's "Open to the Side"): the query tabs stay one list,
//! in order, and each belongs to the left group or the right one. Each group has its own tab in
//! front; one of the two has the focus, and what acts on "the active tab" (Run, ⌘D, find, the
//! result zoom, an agent's query) acts on that group's.
//!
//! Kept in the session (`session.json`) as indexes into its tabs; a file whose split no longer
//! makes sense (a group left empty, an index out of range) reads as no split
//! ([`SplitState::normalized`]): it never keeps the window from opening.

use serde::{Deserialize, Serialize};

/// The left group's share of the width when the split opens (and after a double-click on the
/// seam).
pub const RATIO: f32 = 0.5;
/// The most either group may give up to the other, as a share of the width (the app also keeps
/// a least width in points).
pub const RATIO_MIN: f32 = 0.15;

/// One of the two groups.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Pane {
    #[default]
    Left,
    Right,
}

impl Pane {
    pub fn other(self) -> Pane {
        match self {
            Pane::Left => Pane::Right,
            Pane::Right => Pane::Left,
        }
    }

    pub fn index(self) -> usize {
        match self {
            Pane::Left => 0,
            Pane::Right => 1,
        }
    }
}

/// The split as the session keeps it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SplitState {
    /// The right group's tabs, as indexes into the session's tabs (all the others are the
    /// left's).
    pub right: Vec<usize>,
    /// The right group has the focus: the session's `active` tab is its.
    pub focus_right: bool,
    /// The other group's tab in front (an index into the session's tabs).
    pub other: usize,
    /// The left group's share of the work surface's width.
    pub ratio: f32,
}

impl Default for SplitState {
    fn default() -> SplitState {
        SplitState { right: Vec::new(), focus_right: false, other: 0, ratio: RATIO }
    }
}

impl SplitState {
    /// This split for `n` tabs with `active` in front of the focused group, made to make sense:
    /// indexes in range and once each, both groups holding a tab, `focus_right` saying where
    /// `active` is, `other` in the other group (its first tab when it was not), a finite ratio
    /// within [`RATIO_MIN`]. `None` when there is nothing left to split.
    pub fn normalized(&self, n: usize, active: usize) -> Option<SplitState> {
        if active >= n {
            return None;
        }
        let mut right: Vec<usize> = Vec::new();
        for &i in &self.right {
            if i < n && !right.contains(&i) {
                right.push(i);
            }
        }
        right.sort_unstable();
        if right.is_empty() || right.len() == n {
            return None;
        }
        let focus_right = right.contains(&active);
        let in_other = |i: usize| right.contains(&i) != focus_right;
        let other = if self.other < n && in_other(self.other) { self.other } else { (0..n).find(|&i| in_other(i))? };
        Some(SplitState { right, focus_right, other, ratio: clamp_ratio(self.ratio) })
    }
}

/// `r` as a left share both groups can live with ([`RATIO`] when it is not a number).
pub fn clamp_ratio(r: f32) -> f32 {
    if r.is_finite() { r.clamp(RATIO_MIN, 1. - RATIO_MIN) } else { RATIO }
}

/// The tab that comes to the front of a group when `closed` leaves it: the one before it, else
/// the one after it; `None` when it was the group's last. `group`: the group's tabs in order,
/// `closed` among them.
pub fn neighbour<K: Copy + PartialEq>(group: &[K], closed: K) -> Option<K> {
    let at = group.iter().position(|k| *k == closed)?;
    if at > 0 { Some(group[at - 1]) } else { group.get(1).copied() }
}

/// Moves `item` into a group, at gap `slot` among that group's items (`in_group`), counted the
/// way a tab strip counts them: with `item` where it is when it is in that group already
/// (dropping it either side of itself changes nothing). It lands before the group's item at that
/// gap, or after the group's last one (at the end of the list when the group has none left).
/// False when nothing moved.
pub fn place<K: Copy + PartialEq>(order: &mut Vec<K>, item: K, slot: usize, in_group: impl Fn(&K) -> bool) -> bool {
    let Some(from) = order.iter().position(|k| *k == item) else { return false };
    let mut slot = slot;
    if in_group(&item) {
        let mine = order.iter().filter(|k| in_group(k)).position(|k| *k == item).unwrap_or(0);
        if !crate::order::moves(mine, slot) {
            return false;
        }
        if slot > mine {
            slot -= 1;
        }
    }
    order.remove(from);
    let members: Vec<usize> = order.iter().enumerate().filter(|(_, k)| in_group(k)).map(|(i, _)| i).collect();
    let at = match members.get(slot) {
        Some(&i) => i,
        None => members.last().map_or(order.len(), |&i| i + 1),
    };
    order.insert(at, item);
    true
}

/// The widths of the left and the right group in `total` points (the seam's `seam` points
/// between them), the right group taking `1 - ratio` once open. `open` is how far the split is
/// open (0 … 1); `leaving` is the group that grows in or folds away while it moves (the other one
/// gives it room or takes it back).
pub fn widths(total: f32, ratio: f32, seam: f32, open: f32, leaving: Pane) -> (f32, f32) {
    let room = (total - seam).max(0.);
    let open = open.clamp(0., 1.);
    let ratio = clamp_ratio(ratio);
    match leaving {
        Pane::Right => {
            let right = room * (1. - ratio) * open;
            (room - right, right)
        }
        Pane::Left => {
            let left = room * ratio * open;
            (left, room - left)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_tab_gives_way_to_the_one_before_it() {
        assert_eq!(neighbour(&['a', 'b', 'c'], 'b'), Some('a'));
        assert_eq!(neighbour(&['a', 'b', 'c'], 'a'), Some('b'));
        assert_eq!(neighbour(&['a', 'b', 'c'], 'c'), Some('b'));
        assert_eq!(neighbour(&['a'], 'a'), None);
        assert_eq!(neighbour(&['a'], 'z'), None);
    }

    #[test]
    fn place_moves_within_a_group_like_the_strip() {
        // a b c on the left, X Y on the right, interleaved in the list.
        let right = |k: &char| k.is_uppercase();
        let left = |k: &char| !right(k);
        let mut v = vec!['a', 'X', 'b', 'Y', 'c'];
        // 'a' to the gap after 'b' (gap 2, counted with 'a'): a lands before 'c'.
        assert!(place(&mut v, 'a', 2, left));
        assert_eq!(v.iter().filter(|k| left(k)).collect::<String>(), "bac");
        // Either side of itself: nothing moves.
        let before = v.clone();
        assert!(!place(&mut v, 'a', 1, left));
        assert!(!place(&mut v, 'a', 2, left));
        assert_eq!(v, before);
        // Past the last: after the group's last tab.
        assert!(place(&mut v, 'b', 3, left));
        assert_eq!(v.iter().filter(|k| left(k)).collect::<String>(), "acb");
        assert_eq!(v.iter().filter(|k| right(k)).collect::<String>(), "XY");
    }

    #[test]
    fn place_moves_into_the_other_group() {
        // 'b' (left) joins the right group between X and Y: the caller then counts it there.
        let mut v = vec!['a', 'X', 'b', 'Y', 'c'];
        assert!(place(&mut v, 'b', 1, |k: &char| k.is_uppercase()));
        let pos = |c| v.iter().position(|k| *k == c).unwrap();
        assert!(pos('X') < pos('b') && pos('b') < pos('Y'));
        // Into an empty group: at the end.
        let mut v = vec!['a', 'b'];
        assert!(place(&mut v, 'a', 0, |k: &char| k.is_uppercase()));
        assert_eq!(v, ['b', 'a']);
    }

    #[test]
    fn widths_open_from_the_side_that_moves() {
        // Open half and half: 499.5 each around a 1 pt seam.
        assert_eq!(widths(1000., 0.5, 1., 1., Pane::Right), (499.5, 499.5));
        assert_eq!(widths(1000., 0.5, 1., 1., Pane::Left), (499.5, 499.5));
        // Shut: the staying group has everything.
        assert_eq!(widths(1000., 0.5, 1., 0., Pane::Right), (999., 0.));
        assert_eq!(widths(1000., 0.5, 1., 0., Pane::Left), (0., 999.));
        // Half-way, the right group has half its room.
        let (l, r) = widths(1001., 0.6, 1., 0.5, Pane::Right);
        assert!((r - 200.).abs() < 1e-3 && (l + r - 1000.).abs() < 1e-3);
        // A ratio out of bounds is held to them.
        assert_eq!(widths(101., 0.01, 1., 1., Pane::Right).0, 15.);
    }

    #[test]
    fn a_saved_split_reads_sanely() {
        let s = SplitState { right: vec![2, 2, 9, 1], focus_right: false, other: 0, ratio: f32::NAN };
        // Four tabs, the active one (3) on the left: the other group's front must be on the right.
        let n = s.normalized(4, 3).unwrap();
        assert_eq!(n.right, [1, 2]);
        assert!(!n.focus_right);
        assert_eq!(n.other, 1);
        assert_eq!(n.ratio, RATIO);
        // The active tab says which group has the focus.
        let n = s.normalized(4, 2).unwrap();
        assert!(n.focus_right && n.other == 0);
        // A group left empty, or no tabs: no split.
        assert_eq!(SplitState { right: vec![], ..s.clone() }.normalized(4, 0), None);
        assert_eq!(SplitState { right: vec![0, 1], ..s.clone() }.normalized(2, 0), None);
        assert_eq!(s.normalized(0, 0), None);
        // An old file without the field: the default (no split).
        let old: crate::session::Session = serde_json::from_str(r#"{"version":1,"tabs":[]}"#).unwrap();
        assert_eq!(old.split, None);
    }
}
