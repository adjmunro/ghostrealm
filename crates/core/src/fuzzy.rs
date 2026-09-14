//! A small deterministic fuzzy scorer for the command palette.
//!
//! Subsequence match with bonuses for contiguity and word-boundary starts, so
//! e.g. "nt" ranks "New Tab" above "Print". Good enough for a palette; the
//! registry calls it behind [`crate::Registry::search`], so it can be swapped
//! for a heavier matcher (nucleo, fzf-style) later without touching callers.

/// Score `candidate` against `query`. Higher is better. `None` if `query` is
/// not a subsequence of `candidate` (case-insensitive). An empty query scores 0.
pub fn score(query: &str, candidate: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let q: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    let c: Vec<char> = candidate.chars().flat_map(char::to_lowercase).collect();
    let craw: Vec<char> = candidate.chars().collect();

    let mut qi = 0usize;
    let mut total = 0i32;
    let mut prev_match: Option<usize> = None;
    let mut first_match: Option<usize> = None;

    for (ci, &cc) in c.iter().enumerate() {
        if qi >= q.len() {
            break;
        }
        if cc == q[qi] {
            if first_match.is_none() {
                first_match = Some(ci);
            }
            let mut s = 1;
            // Contiguous with the previous matched char.
            if prev_match == Some(ci.wrapping_sub(1)) {
                s += 5;
            }
            // Word-boundary start (index 0, or preceded by a separator, or a
            // lower->upper camelCase hump in the original text).
            let boundary = ci == 0
                || matches!(
                    c.get(ci - 1),
                    Some(' ') | Some('-') | Some('_') | Some('/') | Some('.')
                )
                || craw
                    .get(ci)
                    .zip(craw.get(ci.wrapping_sub(1)))
                    .is_some_and(|(cur, prev)| cur.is_uppercase() && prev.is_lowercase());
            if boundary {
                s += 10;
            }
            total += s;
            prev_match = Some(ci);
            qi += 1;
        }
    }

    if qi != q.len() {
        return None;
    }

    // Prefer earlier first matches and shorter candidates, gently.
    total -= first_match.unwrap_or(0) as i32;
    total -= (c.len() as i32) / 20;
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::score;

    #[test]
    fn non_subsequence_is_none() {
        assert_eq!(score("xyz", "New Tab"), None);
    }

    #[test]
    fn empty_query_matches_everything() {
        assert_eq!(score("", "anything"), Some(0));
    }

    #[test]
    fn word_boundary_beats_midword() {
        // "nt" as word-boundary initials of "New Tab" should beat "Print".
        let a = score("nt", "New Tab").unwrap();
        let b = score("nt", "Print").unwrap();
        assert!(
            a > b,
            "expected 'New Tab' ({a}) > 'Print' ({b}) for query 'nt'"
        );
    }

    #[test]
    fn contiguous_beats_scattered() {
        // Same characters, no word-boundary confound: contiguity should win.
        let contiguous = score("tab", "xtab").unwrap();
        let scattered = score("tab", "xtxaxb").unwrap();
        assert!(
            contiguous > scattered,
            "contiguous ({contiguous}) should beat scattered ({scattered})"
        );
    }

    #[test]
    fn case_insensitive() {
        assert!(score("NEW", "new tab").is_some());
    }
}
