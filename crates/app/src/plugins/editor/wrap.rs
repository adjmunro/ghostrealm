//! Soft-wrapping editor lines into visual rows.

/// Soft-wrap `line` to `cols` columns, returning each visual row as
/// `(text, start_char_index)`. Greedy word wrap (break at the last space that
/// fits, keeping the space on the current row); a word longer than `cols` is
/// hard-broken. A short line yields a single row.
pub fn wrap_line(line: &str, cols: usize) -> Vec<(String, usize)> {
    let chars: Vec<char> = line.chars().collect();
    if cols == 0 || chars.len() <= cols {
        return vec![(line.to_string(), 0)];
    }
    let mut rows = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let hard_end = (i + cols).min(chars.len());
        let mut brk = hard_end;
        if hard_end < chars.len() {
            // Prefer breaking after the last space within the window.
            if let Some(pos) = (i..hard_end).rev().find(|&k| chars[k] == ' ') {
                if pos + 1 > i {
                    brk = pos + 1;
                }
            }
        }
        rows.push((chars[i..brk].iter().collect(), i));
        i = brk;
    }
    if rows.is_empty() {
        rows.push((String::new(), 0));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_line_word_wraps_with_char_start_offsets() {
        // A short line is a single row.
        assert_eq!(wrap_line("hello", 10), vec![("hello".to_string(), 0)]);
        // Greedy word wrap keeps the breaking space and reports char start offsets.
        assert_eq!(
            wrap_line("the quick brown", 9),
            vec![
                ("the ".to_string(), 0),
                ("quick ".to_string(), 4),
                ("brown".to_string(), 10),
            ]
        );
        // A word longer than the width is hard-broken.
        assert_eq!(
            wrap_line("abcdef", 3),
            vec![("abc".to_string(), 0), ("def".to_string(), 3)]
        );
    }
}
