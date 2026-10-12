//! Line-level change counts between two versions of a text: how many lines a
//! minimal diff adds and deletes (Myers' algorithm, distance only).

/// Most edits [`line_changes`] searches for before giving up on a minimal
/// diff (the search costs O((N+M)·D)).
const MAX_EDITS: usize = 4000;

/// Lines `(added, deleted)` going from `old` to `new`. Past [`MAX_EDITS`] the
/// differing middle (after the common head and tail) counts as all replaced.
pub fn line_changes<T: PartialEq>(old: &[T], new: &[T]) -> (usize, usize) {
    let head = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let (old, new) = (&old[head..], &new[head..]);
    let tail = old.iter().rev().zip(new.iter().rev()).take_while(|(a, b)| a == b).count();
    let (old, new) = (&old[..old.len() - tail], &new[..new.len() - tail]);
    match edit_distance(old, new, MAX_EDITS) {
        Some(d) => {
            let common = (old.len() + new.len() - d) / 2;
            (new.len() - common, old.len() - common)
        }
        None => (new.len(), old.len()),
    }
}

/// The fewest line insertions plus deletions turning `a` into `b`, if at most
/// `max`.
fn edit_distance<T: PartialEq>(a: &[T], b: &[T], max: usize) -> Option<usize> {
    let (n, m) = (a.len(), b.len());
    let max = max.min(n + m);
    // v[k + off]: the furthest x reached on diagonal k (= x - y).
    let off = max + 1;
    let mut v = vec![0usize; 2 * max + 3];
    for d in 0..=max {
        for k in (0..=2 * d).map(|i| i as isize - d as isize).step_by(2) {
            let at = |k: isize| (k + off as isize) as usize;
            let down = k == -(d as isize) || (k != d as isize && v[at(k - 1)] < v[at(k + 1)]);
            let mut x = if down { v[at(k + 1)] } else { v[at(k - 1)] + 1 };
            let mut y = (x as isize - k) as usize;
            while x < n && y < m && a[x] == b[y] {
                x += 1;
                y += 1;
            }
            v[at(k)] = x;
            if x >= n && y >= m {
                return Some(d);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<&str> {
        s.lines().collect()
    }

    #[test]
    fn counts_adds_deletes_and_changes() {
        assert_eq!(line_changes(&lines("a\nb\nc"), &lines("a\nb\nc")), (0, 0));
        assert_eq!(line_changes(&lines("a\nc"), &lines("a\nb\nc")), (1, 0));
        assert_eq!(line_changes(&lines("a\nb\nc"), &lines("a\nc")), (0, 1));
        assert_eq!(line_changes(&lines("a\nb\nc"), &lines("a\nB\nc")), (1, 1), "a change is both");
        assert_eq!(line_changes::<&str>(&[], &lines("x\ny")), (2, 0));
        assert_eq!(line_changes(&lines("x\ny"), &[]), (0, 2));
    }

    #[test]
    fn finds_the_minimal_diff_not_just_head_and_tail() {
        let old = lines("a\nb\nc\nd\ne\nf");
        let new = lines("a\nx\nc\nd\ny\nf\ng");
        assert_eq!(line_changes(&old, &new), (3, 2));
        // Moving one line is a delete and an add, not a rewrite of everything.
        assert_eq!(line_changes(&lines("1\n2\n3\n4"), &lines("2\n3\n4\n1")), (1, 1));
    }

    #[test]
    fn past_the_limit_the_middle_counts_as_replaced() {
        let old: Vec<u32> = (0..3000).collect();
        let new: Vec<u32> = (10_000..13_000).collect();
        assert_eq!(line_changes(&old, &new), (3000, 3000));
    }
}
