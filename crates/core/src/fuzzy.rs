//! A small deterministic fuzzy scorer for the command palette.
//!
//! Subsequence match with bonuses for contiguity and word-boundary starts, so
//! e.g. "nt" ranks "New Tab" above "Print". Good enough for a palette; the
//! registry calls it behind [`crate::Registry::search`], so it can be swapped
//! for a heavier matcher (nucleo, fzf-style) later without touching callers.

/// Score `candidate` against `query`, case-insensitively. Higher is better;
/// `None` if `query` is not a subsequence of `candidate`. An empty query scores 0.
pub fn score(query: &str, candidate: &str) -> Option<i32> {
    score_cased(query, candidate, false).map(|(s, _)| s)
}

/// Smart-case scoring: case-sensitive when `query` contains any uppercase letter,
/// otherwise case-insensitive (the common "type lowercase to match anything,
/// add a capital to pin it" behaviour).
pub fn score_smart(query: &str, candidate: &str) -> Option<i32> {
    match_smart(query, candidate).map(|(s, _)| s)
}

/// Smart-case match returning both a score and the candidate char indices that
/// matched (for highlighting). A `*` in the query switches to wildcard matching:
/// the segments between stars must appear in order as substrings.
pub fn match_smart(query: &str, candidate: &str) -> Option<(i32, Vec<usize>)> {
    let case_sensitive = query.chars().any(|c| c.is_uppercase());
    if query.contains('*') {
        wildcard_cased(query, candidate, case_sensitive)
    } else {
        score_cased(query, candidate, case_sensitive)
    }
}

fn fold(s: &str, case_sensitive: bool) -> Vec<char> {
    if case_sensitive {
        s.chars().collect()
    } else {
        s.chars().flat_map(char::to_lowercase).collect()
    }
}

fn score_cased(query: &str, candidate: &str, case_sensitive: bool) -> Option<(i32, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let q = fold(query, case_sensitive);
    let c = fold(candidate, case_sensitive);
    let craw: Vec<char> = candidate.chars().collect();

    let mut qi = 0usize;
    let mut total = 0i32;
    let mut prev_match: Option<usize> = None;
    let mut first_match: Option<usize> = None;
    let mut indices: Vec<usize> = Vec::with_capacity(q.len());

    for (ci, &cc) in c.iter().enumerate() {
        if qi >= q.len() {
            break;
        }
        if cc == q[qi] {
            if first_match.is_none() {
                first_match = Some(ci);
            }
            let mut s = 1;
            if prev_match == Some(ci.wrapping_sub(1)) {
                s += 5;
            }
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
            indices.push(ci);
            prev_match = Some(ci);
            qi += 1;
        }
    }

    if qi != q.len() {
        return None;
    }
    total -= first_match.unwrap_or(0) as i32;
    total -= (c.len() as i32) / 20;
    Some((total, indices))
}

/// Match a `*`-wildcard `query`: the segments between stars must appear, in order,
/// as contiguous substrings anywhere in the candidate — an implied wildcard at both
/// ends. So `*.k` matches `main.kt` (contains `.k`), `foo*bar` matches `foobarbaz`,
/// and a bare `*` matches everything. Returns a score and the matched char indices.
fn wildcard_cased(query: &str, candidate: &str, case_sensitive: bool) -> Option<(i32, Vec<usize>)> {
    let segs: Vec<Vec<char>> = query
        .split('*')
        .filter(|s| !s.is_empty())
        .map(|s| fold(s, case_sensitive))
        .collect();
    let c = fold(candidate, case_sensitive);
    if segs.is_empty() {
        return Some((0, Vec::new()));
    }
    let mut pos = 0usize;
    let mut indices: Vec<usize> = Vec::new();
    let mut first = None;
    for seg in &segs {
        let at = find_sub(&c, seg, pos)?;
        if first.is_none() {
            first = Some(at);
        }
        indices.extend(at..at + seg.len());
        pos = at + seg.len();
    }
    let score = 50 - first.unwrap_or(0) as i32 - (c.len() as i32) / 20;
    Some((score, indices))
}

/// Index of the first occurrence of `needle` in `hay` at or after `from`.
fn find_sub(hay: &[char], needle: &[char], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from);
    }
    if needle.len() > hay.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| hay[i..i + needle.len()] == needle[..])
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

    #[test]
    fn wildcard_is_unanchored_contains_in_order() {
        use super::match_smart;
        // Implied wildcard at both ends: segments just need to appear in order.
        assert!(match_smart("*.k", "main.kt").is_some(), "*.k matches .kt files");
        assert!(match_smart("*.kt", "main.kt").is_some());
        assert!(match_smart("*out*", "checkout.rs").is_some());
        assert!(match_smart("a*b", "axxb").is_some());
        assert!(match_smart("a*b", "bxa").is_none(), "order matters");
        assert!(match_smart("foo*bar", "foobarbaz").is_some());
        assert!(match_smart("src*", "my/src").is_some(), "no start anchor");
        assert!(match_smart("*.md", "main.kt").is_none());
    }

    #[test]
    fn smart_case_is_sensitive_only_with_an_uppercase() {
        use super::score_smart;
        // All-lowercase query: case-insensitive, matches either case.
        assert!(score_smart("read", "README").is_some());
        assert!(score_smart("read", "readme").is_some());
        // An uppercase in the query pins case: "RE" matches README, not readme.
        assert!(score_smart("RE", "README").is_some());
        assert!(score_smart("RE", "readme").is_none());
    }
}
