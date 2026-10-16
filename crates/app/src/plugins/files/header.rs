//! The document header: the details of the file a document shows (its name,
//! kind and size, and when it was modified) above it, with the document's
//! own controls (its ribbon icons and `⋯`) at the right of the name's line.
//! A document paints its controls first, right to left, then the header's
//! text up to where they start.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ghostrealm_core::Rect;

use crate::plugin::{theme, Font, PaintCx, UiMetrics};

/// What a header says about its document.
pub struct HeaderText<'a> {
    /// The file name, or a scratch document's title.
    pub name: &'a str,
    pub path: Option<&'a Path>,
    /// What the file is ("MD text", "PNG image").
    pub kind: &'a str,
    /// Unsaved edits: a dot follows the name, as in its file tab.
    pub modified: bool,
    /// The file changed on disk under unsaved edits: the name shows amber.
    pub changed_on_disk: bool,
    /// Facts after the kind and size (an image's size in pixels, a track's
    /// length).
    pub extra: Option<String>,
}

/// A file's size and modification time, as last read.
#[derive(Clone, Copy, Default)]
struct Stat {
    size: Option<u64>,
    modified: Option<SystemTime>,
}

/// A document's header. It shows the file's size and modification time as
/// last read ([`DocHeader::refresh`]): its document reads them again when
/// it shows, saves, reloads, or follows its file, never while painting.
#[derive(Default)]
pub struct DocHeader {
    stat: Option<(PathBuf, Stat)>,
}

impl DocHeader {
    /// The header's height: the name, a line of facts, and the modified line.
    pub fn height(ui: UiMetrics) -> f32 {
        let pad = Self::pad(ui);
        (pad.1 * 2.0 + ui.cell_h * 1.4 + ui.cell_h * 1.2 + ui.cell_h).round()
    }

    /// The header's padding: (sides, top and bottom).
    fn pad(ui: UiMetrics) -> (f32, f32) {
        ((12.0 * ui.scale).round(), (8.0 * ui.scale).round())
    }

    /// The row the document's controls sit in, across `header`: centred on
    /// the name's line.
    pub fn control_row(ui: UiMetrics, header: Rect) -> Rect {
        let (side, top) = Self::pad(ui);
        let h = (ui.cell_h + 8.0 * ui.scale).round();
        let y = (header.y + top + ui.cell_h * 0.5 - h * 0.5).round();
        Rect { x: header.x + side * 0.5, y, w: (header.w - side).max(1.0), h }
    }

    /// Read `path`'s size and modification time again (`None`: a scratch
    /// document, which has neither).
    pub fn refresh(&mut self, path: Option<&Path>) {
        self.stat = path.map(|path| {
            let meta = std::fs::metadata(path).ok();
            let stat = Stat {
                size: meta.as_ref().filter(|m| m.is_file()).map(|m| m.len()),
                modified: meta.and_then(|m| m.modified().ok()),
            };
            (path.to_path_buf(), stat)
        });
    }

    /// The facts last read of `path`; none when they were read of another.
    fn stat(&self, path: Option<&Path>) -> Option<Stat> {
        match (&self.stat, path) {
            (Some((p, stat)), Some(path)) if p == path => Some(*stat),
            _ => None,
        }
    }

    /// Paint the header into `r`: its background, then `text` clipped left of
    /// `right` (where the document's controls start). With `name_button`,
    /// the name of a file is a button with that id (it opens the file menu).
    /// Returns the name's rect.
    pub fn paint(&mut self, cx: &mut PaintCx, r: Rect, right: f32, text: &HeaderText, name_button: Option<u32>) -> Rect {
        let ui = cx.ui;
        let ch = ui.cell_h;
        let (side, top) = Self::pad(ui);
        cx.fill(r, cx.chrome.sidebar);
        let line = theme::hairline(ui);
        cx.fill(Rect { x: r.x, y: r.y + r.h - line, w: r.w, h: line }, theme::INPUT_BORDER);
        let x = r.x + side;
        let clip = Rect { x: r.x, y: r.y, w: (right - r.x).max(0.0), h: r.h };
        let mut y = r.y + top;

        let font = Font::SANS.bold(true);
        let name = cx.shape(text.name, font, clip.w);
        let name_rect = Rect {
            x: x - side * 0.5,
            y: (y - 4.0 * ui.scale).round(),
            w: (name.width + side).min((clip.x + clip.w - x + side * 0.5).max(0.0)),
            h: (ch + 8.0 * ui.scale).round(),
        };
        if let (Some(id), Some(_)) = (name_button, text.path) {
            if cx.button(name_rect, id) {
                cx.round_rect(name_rect, theme::box_radius(ui), theme::HOVER_BG, theme::HOVER_ALPHA, None);
            }
            let tip = match text.changed_on_disk {
                true => "Changed on disk since you opened it\nFile actions",
                false => "File actions",
            };
            cx.tooltip(name_rect, tip);
        }
        let color = if text.changed_on_disk { theme::WARN } else { theme::TEXT };
        cx.place(name, x, y, clip, color);
        if text.modified {
            let d = (ch * 0.45).round();
            let dot = Rect { x: (x + name.width + side * 0.5).round(), y: y + ((ch - d) * 0.5).round(), w: d, h: d };
            if dot.x + dot.w <= clip.x + clip.w {
                cx.round_rect(dot, d * 0.5, theme::TEXT, 1.0, None);
            }
        }
        y += ch * 1.4;

        let stat = self.stat(text.path);
        let mut facts = vec![text.kind.to_string()];
        facts.extend(stat.and_then(|s| s.size).map(human_size));
        facts.extend(text.extra.clone());
        // Only the name's line holds controls: the rest runs the full width.
        let full = Rect { x: r.x, y: r.y, w: (r.w - side).max(0.0), h: r.h };
        cx.label(&facts.join("  \u{00b7}  "), Font::SANS, x, y, full, theme::TEXT_DIM);
        y += ch * 1.2;
        let when = match (text.path, stat) {
            (None, _) => "Not saved".to_string(),
            (Some(_), Some(Stat { modified: Some(m), .. })) => format!("Modified {}", ago(m)),
            (Some(_), Some(_)) => "Not on disk".to_string(),
            (Some(_), None) => String::new(),
        };
        cx.label(&when, Font::SANS, x, y, full, theme::TEXT_DIM);
        name_rect
    }
}

/// What a text file is: its extension and "text" ("MD text"), or "Text".
pub fn text_kind(path: Option<&Path>) -> String {
    match path.and_then(Path::extension) {
        Some(ext) => format!("{} text", ext.to_string_lossy().to_uppercase()),
        None => "Text".to_string(),
    }
}

/// A byte count the way Finder shows it (powers of 1000).
pub fn human_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if n < 1000 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1000.0;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if v < 10.0 {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{v:.0} {}", UNITS[unit])
    }
}

/// How long ago `t` was, in its largest whole unit ("3 days ago").
pub fn ago(t: SystemTime) -> String {
    let Ok(d) = SystemTime::now().duration_since(t) else {
        return "just now".to_string();
    };
    let s = d.as_secs();
    let (n, unit) = match s {
        0..60 => return "just now".to_string(),
        60..3600 => (s / 60, "minute"),
        3600..86_400 => (s / 3600, "hour"),
        86_400..2_592_000 => (s / 86_400, "day"),
        2_592_000..31_536_000 => (s / 2_592_000, "month"),
        _ => (s / 31_536_000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use crate::plugin::testing::{tmpdir, Harness, UI};
    use crate::plugin::View;

    #[test]
    fn formats_sizes_ages_and_kinds() {
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1_234), "1.2 KB");
        assert_eq!(human_size(45_600_000), "46 MB");
        let now = SystemTime::now();
        assert_eq!(ago(now), "just now");
        assert_eq!(ago(now - Duration::from_secs(3 * 86_400 + 5)), "3 days ago");
        assert_eq!(ago(now - Duration::from_secs(3_700)), "1 hour ago");
        assert_eq!(text_kind(Some(Path::new("a/b.md"))), "MD text");
        assert_eq!(text_kind(Some(Path::new("Makefile"))), "Text");
        assert_eq!(text_kind(None), "Text");
    }

    /// A view that is only a header, for painting one: of `a.md`, else of
    /// the name given, modified or not.
    struct Only(DocHeader, PathBuf, Option<&'static str>, bool);

    impl View for Only {
        fn plugin(&self) -> &'static str {
            "test"
        }
        fn paint(&mut self, cx: &mut PaintCx, rect: Rect) {
            let text = HeaderText {
                name: self.2.unwrap_or("a.md"),
                path: Some(&self.1),
                kind: "MD text",
                modified: self.3,
                changed_on_disk: false,
                extra: None,
            };
            self.0.paint(cx, rect, rect.x + rect.w, &text, Some(7));
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
    }

    #[test]
    fn the_header_shows_the_name_the_facts_and_the_age_and_the_name_is_a_button() {
        let d = tmpdir("doc-header");
        std::fs::write(d.join("a.md"), "x".repeat(4100)).unwrap();
        let mut v = Only(DocHeader::default(), d.join("a.md"), None, false);
        v.0.refresh(Some(&v.1));
        let mut h = Harness::new();
        let r = Rect { x: 0.0, y: 0.0, w: 400.0, h: DocHeader::height(UI) };
        let frame = h.paint(&mut v, r);
        assert!(frame.view_buttons.iter().any(|b| b.2 == 7), "the name opens the file menu");
        let texts = h.texts();
        for want in ["a.md", "MD text  \u{00b7}  4.1 KB", "Modified just now"] {
            assert!(texts.iter().any(|t| t == want), "{want:?} in {texts:?}");
        }
    }

    /// The quads of a dot `side` across, as the unsaved marker draws one.
    fn dots(frame: &crate::plugin::Frame, side: f32) -> usize {
        frame.bg.iter().filter(|q| q.px == [side, side] && q.radius == side * 0.5).count()
    }

    #[test]
    fn unsaved_shows_a_dot_after_the_name_not_italics() {
        let d = tmpdir("doc-header-dot");
        std::fs::write(d.join("a.md"), "x").unwrap();
        let mut h = Harness::new();
        let r = Rect { x: 0.0, y: 0.0, w: 400.0, h: DocHeader::height(UI) };
        let side = (UI.cell_h * 0.45).round();
        let mut v = Only(DocHeader::default(), d.join("a.md"), None, false);
        assert_eq!(dots(h.paint(&mut v, r), side), 0);
        v.3 = true;
        assert_eq!(dots(h.paint(&mut v, r), side), 1);
    }

    #[test]
    fn a_right_to_left_name_starts_at_the_left() {
        let d = tmpdir("doc-header-rtl");
        let mut v = Only(DocHeader::default(), d.join("x.md"), Some("\u{05e9}\u{05dc}\u{05d5}\u{05dd}.md"), false);
        let mut h = Harness::new();
        let r = Rect { x: 0.0, y: 0.0, w: 400.0, h: DocHeader::height(UI) };
        h.paint(&mut v, r);
        let left = h.text_left("\u{05e9}\u{05dc}\u{05d5}\u{05dd}.md").expect("the name was drawn");
        assert!(left < 1.0, "glyphs start at {left}, not against the right of the box");
    }
}

