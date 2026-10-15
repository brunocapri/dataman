//! Reordering a list by drag (query tabs, saved connections): the slot a pointer points at and
//! the move a drop there makes. A slot is a gap: 0 before the first item, `n` after the last.

/// The slot a pointer at `pos` points at among items whose middles are `mids` (in order): the
/// gap before the first middle it has not passed.
pub fn slot(mids: &[f32], pos: f32) -> usize {
    mids.iter().take_while(|m| pos > **m).count()
}

/// Whether item `from` dropped at `slot` moves (the gaps either side of it leave it in place).
pub fn moves(from: usize, slot: usize) -> bool {
    slot != from && slot != from + 1
}

/// Moves item `from` to `slot` (counted before the move): the index it lands at, `None` when it
/// stays where it is.
pub fn move_to<T>(v: &mut Vec<T>, from: usize, slot: usize) -> Option<usize> {
    if from >= v.len() || slot > v.len() || !moves(from, slot) {
        return None;
    }
    let item = v.remove(from);
    let to = if slot > from { slot - 1 } else { slot };
    v.insert(to, item);
    Some(to)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_counts_the_middles_passed() {
        let mids = [10., 30., 50.];
        assert_eq!(slot(&mids, 0.), 0);
        assert_eq!(slot(&mids, 10.), 0);
        assert_eq!(slot(&mids, 11.), 1);
        assert_eq!(slot(&mids, 49.), 2);
        assert_eq!(slot(&mids, 90.), 3);
        assert_eq!(slot(&[], 5.), 0);
    }

    #[test]
    fn move_to_lands_in_the_gap() {
        let mut v = vec!['a', 'b', 'c', 'd'];
        assert_eq!(move_to(&mut v, 0, 3), Some(2));
        assert_eq!(v, ['b', 'c', 'a', 'd']);
        assert_eq!(move_to(&mut v, 3, 0), Some(0));
        assert_eq!(v, ['d', 'b', 'c', 'a']);
        assert_eq!(move_to(&mut v, 1, 4), Some(3));
        assert_eq!(v, ['d', 'c', 'a', 'b']);
        // Either side of itself, or out of range: nothing moves.
        assert_eq!(move_to(&mut v, 2, 2), None);
        assert_eq!(move_to(&mut v, 2, 3), None);
        assert_eq!(move_to(&mut v, 4, 0), None);
        assert_eq!(move_to(&mut v, 0, 5), None);
        assert_eq!(v, ['d', 'c', 'a', 'b']);
        assert!(!moves(1, 1) && !moves(1, 2) && moves(1, 0) && moves(1, 3));
    }
}
