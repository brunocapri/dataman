//! A session file whose split no longer makes sense (a group left empty, an index out of
//! range) reads as no split: it never keeps the window from opening.

use serde::{Deserialize, Serialize};

pub const RATIO: f32 = 0.5;
pub const RATIO_MIN: f32 = 0.15;

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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SplitState {
    pub right: Vec<usize>,
    pub focus_right: bool,
    pub other: usize,
    pub ratio: f32,
}

impl Default for SplitState {
    fn default() -> SplitState {
        SplitState { right: Vec::new(), focus_right: false, other: 0, ratio: RATIO }
    }
}

impl SplitState {
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

pub fn clamp_ratio(r: f32) -> f32 {
    if r.is_finite() { r.clamp(RATIO_MIN, 1. - RATIO_MIN) } else { RATIO }
}

pub fn neighbour<K: Copy + PartialEq>(group: &[K], closed: K) -> Option<K> {
    let at = group.iter().position(|k| *k == closed)?;
    if at > 0 { Some(group[at - 1]) } else { group.get(1).copied() }
}

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
        let right = |k: &char| k.is_uppercase();
        let left = |k: &char| !right(k);
        let mut v = vec!['a', 'X', 'b', 'Y', 'c'];
        assert!(place(&mut v, 'a', 2, left));
        assert_eq!(v.iter().filter(|k| left(k)).collect::<String>(), "bac");
        let before = v.clone();
        assert!(!place(&mut v, 'a', 1, left));
        assert!(!place(&mut v, 'a', 2, left));
        assert_eq!(v, before);
        assert!(place(&mut v, 'b', 3, left));
        assert_eq!(v.iter().filter(|k| left(k)).collect::<String>(), "acb");
        assert_eq!(v.iter().filter(|k| right(k)).collect::<String>(), "XY");
    }

    #[test]
    fn place_moves_into_the_other_group() {
        let mut v = vec!['a', 'X', 'b', 'Y', 'c'];
        assert!(place(&mut v, 'b', 1, |k: &char| k.is_uppercase()));
        let pos = |c| v.iter().position(|k| *k == c).unwrap();
        assert!(pos('X') < pos('b') && pos('b') < pos('Y'));
        let mut v = vec!['a', 'b'];
        assert!(place(&mut v, 'a', 0, |k: &char| k.is_uppercase()));
        assert_eq!(v, ['b', 'a']);
    }

    #[test]
    fn widths_open_from_the_side_that_moves() {
        assert_eq!(widths(1000., 0.5, 1., 1., Pane::Right), (499.5, 499.5));
        assert_eq!(widths(1000., 0.5, 1., 1., Pane::Left), (499.5, 499.5));
        assert_eq!(widths(1000., 0.5, 1., 0., Pane::Right), (999., 0.));
        assert_eq!(widths(1000., 0.5, 1., 0., Pane::Left), (0., 999.));
        let (l, r) = widths(1001., 0.6, 1., 0.5, Pane::Right);
        assert!((r - 200.).abs() < 1e-3 && (l + r - 1000.).abs() < 1e-3);
        assert_eq!(widths(101., 0.01, 1., 1., Pane::Right).0, 15.);
    }

    #[test]
    fn a_saved_split_reads_sanely() {
        let s = SplitState { right: vec![2, 2, 9, 1], focus_right: false, other: 0, ratio: f32::NAN };
        let n = s.normalized(4, 3).unwrap();
        assert_eq!(n.right, [1, 2]);
        assert!(!n.focus_right);
        assert_eq!(n.other, 1);
        assert_eq!(n.ratio, RATIO);
        let n = s.normalized(4, 2).unwrap();
        assert!(n.focus_right && n.other == 0);
        assert_eq!(SplitState { right: vec![], ..s.clone() }.normalized(4, 0), None);
        assert_eq!(SplitState { right: vec![0, 1], ..s.clone() }.normalized(2, 0), None);
        assert_eq!(s.normalized(0, 0), None);
        let old: crate::session::Session = serde_json::from_str(r#"{"version":1,"tabs":[]}"#).unwrap();
        assert_eq!(old.split, None);
    }
}
