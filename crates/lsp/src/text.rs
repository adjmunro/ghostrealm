//! Positions in text. LSP addresses text by line and a column counted in the
//! negotiated [`Encoding`]'s code units; editors and parsers use byte offsets
//! or characters. These convert between them.

use std::path::Path;

use lsp_types::{Position, PositionEncodingKind, Uri};

/// The unit LSP columns count, agreed during `initialize`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    /// The protocol default: every server must support it.
    #[default]
    Utf16,
    Utf32,
}

impl Encoding {
    pub fn from_kind(kind: &PositionEncodingKind) -> Option<Self> {
        match kind.as_str() {
            "utf-8" => Some(Encoding::Utf8),
            "utf-16" => Some(Encoding::Utf16),
            "utf-32" => Some(Encoding::Utf32),
            _ => None,
        }
    }

    pub fn kind(self) -> PositionEncodingKind {
        match self {
            Encoding::Utf8 => PositionEncodingKind::UTF8,
            Encoding::Utf16 => PositionEncodingKind::UTF16,
            Encoding::Utf32 => PositionEncodingKind::UTF32,
        }
    }

    fn units(self, c: char) -> u32 {
        match self {
            Encoding::Utf8 => c.len_utf8() as u32,
            Encoding::Utf16 => c.len_utf16() as u32,
            Encoding::Utf32 => 1,
        }
    }
}

/// The column (in `enc` units) of the character at index `chars` in `line`.
pub fn units_of_chars(line: &str, chars: usize, enc: Encoding) -> u32 {
    line.chars().take(chars).map(|c| enc.units(c)).sum()
}

/// The character index in `line` at column `units` (in `enc` units). A column
/// inside a character lands on that character; past the end, on the end.
pub fn chars_of_units(line: &str, units: u32, enc: Encoding) -> usize {
    let mut seen = 0;
    for (i, c) in line.chars().enumerate() {
        seen += enc.units(c);
        if seen > units {
            return i;
        }
    }
    line.chars().count()
}

/// The text of line `n` (0-based) without its terminator, and its byte offset.
fn line_at(text: &str, n: u32) -> Option<(usize, &str)> {
    let mut start = 0;
    for (i, line) in text.split('\n').enumerate() {
        if i == n as usize {
            return Some((start, line.strip_suffix('\r').unwrap_or(line)));
        }
        start += line.len() + 1;
    }
    None
}

/// The byte offset of `pos` in `text`. A column past the end of its line lands
/// at the line's end; a line past the end of the text is `None`.
pub fn offset(text: &str, pos: Position, enc: Encoding) -> Option<usize> {
    let (start, line) = line_at(text, pos.line)?;
    let chars = chars_of_units(line, pos.character, enc);
    let bytes: usize = line.chars().take(chars).map(char::len_utf8).sum();
    Some(start + bytes)
}

/// The position of byte `offset` in `text` (clamped to the text; an offset
/// inside a character lands on that character).
pub fn position(text: &str, offset: usize, enc: Encoding) -> Position {
    let offset = offset.min(text.len());
    let before = &text[..floor_char_boundary(text, offset)];
    let line = before.matches('\n').count() as u32;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let character = before[line_start..].chars().map(|c| enc.units(c)).sum();
    Position { line, character }
}

fn floor_char_boundary(text: &str, mut i: usize) -> usize {
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// A `file://` URI for an absolute path, percent-encoding everything but
/// unreserved characters and `/`.
pub fn file_uri(path: &Path) -> Uri {
    let mut s = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                s.push(b as char)
            }
            _ => s.push_str(&format!("%{b:02X}")),
        }
    }
    s.parse().expect("a percent-encoded file path is a valid URI")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_count_in_the_negotiated_unit() {
        // 'é' is 2 UTF-8 bytes, 1 UTF-16 unit; '😀' is 4 bytes, 2 UTF-16 units.
        let line = "é😀x";
        assert_eq!(units_of_chars(line, 3, Encoding::Utf8), 7);
        assert_eq!(units_of_chars(line, 3, Encoding::Utf16), 4);
        assert_eq!(units_of_chars(line, 3, Encoding::Utf32), 3);
        assert_eq!(chars_of_units(line, 3, Encoding::Utf16), 2);
        assert_eq!(chars_of_units(line, 2, Encoding::Utf16), 1, "inside the emoji");
        assert_eq!(chars_of_units(line, 99, Encoding::Utf8), 3);
    }

    #[test]
    fn offsets_and_positions_round_trip() {
        let text = "ab\r\né😀x\n\nlast";
        for enc in [Encoding::Utf8, Encoding::Utf16, Encoding::Utf32] {
            for off in [0, 1, 4, 6, 10, 11, 12, text.len()] {
                let pos = position(text, off, enc);
                assert_eq!(offset(text, pos, enc), Some(off), "{enc:?} {off} {pos:?}");
            }
        }
        let p = |line, character| Position { line, character };
        assert_eq!(position(text, 10, Encoding::Utf16), p(1, 3));
        assert_eq!(offset(text, p(0, 99), Encoding::Utf16), Some(2), "clamped to the line");
        assert_eq!(offset(text, p(9, 0), Encoding::Utf16), None);
    }

    #[test]
    fn file_uris_are_percent_encoded() {
        let uri = file_uri(Path::new("/Users/me/my config/ghost#realm.toml"));
        assert_eq!(uri.as_str(), "file:///Users/me/my%20config/ghost%23realm.toml");
    }
}
