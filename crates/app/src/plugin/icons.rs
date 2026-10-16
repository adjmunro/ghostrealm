//! Icons drawn from quads, so they need no image assets or font coverage, scale
//! with the cell size, and take the theme's colours.
//!
//! Each icon paints into a square (the largest that fits the rect passed,
//! centred), laid out on a 16-unit grid; give it about a row's height. The
//! square sits on whole pixels, and filled and outlined rects snap to them so
//! their edges stay crisp; strokes, dots, and rings don't snap, and are
//! antialiased quads ([`PaintCx::line`], [`PaintCx::round_rect`]), one per
//! segment or shape.
//!
//! [`FileKind::of`] picks a file's icon and [`FileKind::paint`] draws it in the
//! kind's colour; [`Icon::paint`] draws a UI icon in the colour given
//! (`theme::GLYPH`, `theme::GLYPH_HOVER`).

use std::path::Path;

use ghostrealm_core::highlight;
use ghostrealm_core::Rect;

use super::paint::PaintCx;
use super::theme;

/// What a file is, for its icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Markdown,
    /// Plain text, and anything not recognised.
    Text,
    /// Source code or config with a highlighter or known language (`</>`).
    Code,
    /// JSON (`{}`).
    Json,
    /// Git's own files ([`GIT_NAMES`]).
    Git,
    Image,
    Audio,
    Rust,
    Folder { open: bool },
}

const IMAGE_EXTS: &[&str] = &[
    "bmp", "gif", "heic", "ico", "jpeg", "jpg", "png", "svg", "tif", "tiff", "webp",
];
const AUDIO_EXTS: &[&str] = &["aac", "aiff", "flac", "m4a", "mp3", "oga", "ogg", "opus", "wav"];
/// Code with no highlighter or [`highlight::LANGUAGES`] entry yet.
const CODE_EXTS: &[&str] = &["cmake", "gradle", "ini", "mk", "php", "proto"];
/// Git's own files.
const GIT_NAMES: &[&str] = &[".git", ".gitattributes", ".gitignore", ".gitkeep", ".gitmodules"];
/// Code files named without an extension.
const CODE_NAMES: &[&str] = &["dockerfile", "justfile", "makefile", "rakefile", "gemfile"];

impl FileKind {
    /// The kind of the file at `path`, from its name alone (no file system
    /// access). A directory is [`FileKind::Folder`]; the caller knows which
    /// rows are directories.
    pub fn of(path: &Path) -> FileKind {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if GIT_NAMES.contains(&name.as_str()) {
            return FileKind::Git;
        }
        if name.starts_with("dockerfile.") {
            return FileKind::Code;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match ext.as_str() {
            "md" | "markdown" | "mdx" => FileKind::Markdown,
            "json" | "jsonc" | "json5" => FileKind::Json,
            "rs" => FileKind::Rust,
            e if IMAGE_EXTS.contains(&e) => FileKind::Image,
            e if AUDIO_EXTS.contains(&e) => FileKind::Audio,
            e if CODE_EXTS.contains(&e) => FileKind::Code,
            _ if CODE_NAMES.contains(&name.as_str()) => FileKind::Code,
            _ if highlight::for_path(path).is_some() || highlight::language_for_path(path).is_some() => {
                FileKind::Code
            }
            _ => FileKind::Text,
        }
    }

    /// The icon's colour.
    pub fn color(self) -> [u8; 3] {
        match self {
            FileKind::Markdown => [95, 170, 105],
            FileKind::Text => [150, 150, 162],
            FileKind::Code => theme::LABEL,
            FileKind::Json => [220, 145, 65],
            FileKind::Git => [230, 110, 70],
            FileKind::Image => [160, 125, 210],
            FileKind::Audio => [210, 110, 160],
            FileKind::Rust => [205, 120, 80],
            FileKind::Folder { .. } => [110, 150, 200],
        }
    }

    /// Draw the icon into (a square centred in) `r`, under the text.
    pub fn paint(self, cx: &mut PaintCx, r: Rect) {
        let c = self.color();
        let mut p = Pen::new(cx, r);
        match self {
            FileKind::Markdown => {
                p.doc(c);
                let ink = shade(c, 0.45);
                p.rect(5.5, 8.0, 5.0, p.stroke_units(), 0.0, ink, 1.0);
                p.rect(5.5, 10.5, 3.5, p.stroke_units(), 0.0, ink, 1.0);
            }
            FileKind::Text => p.doc(c),
            FileKind::Code => p.code(c),
            FileKind::Json => {
                p.polyline(&[(7.0, 2.5), (6.0, 3.5), (6.0, 6.8), (4.5, 8.0), (6.0, 9.2), (6.0, 12.5), (7.0, 13.5)], c);
                p.polyline(&[(9.0, 2.5), (10.0, 3.5), (10.0, 6.8), (11.5, 8.0), (10.0, 9.2), (10.0, 12.5), (9.0, 13.5)], c);
            }
            FileKind::Git => {
                p.diamond(8.0, 8.0, 7.0, c);
                let mark = [250, 250, 250];
                p.line((6.5, 5.5), (6.5, 10.5), mark);
                p.line((6.5, 6.5), (9.8, 8.6), mark);
                p.dot(6.5, 10.5, 2.4, mark);
                p.dot(9.8, 8.6, 2.4, mark);
            }
            FileKind::Image => {
                p.outline(2.0, 3.0, 12.0, 10.0, 1.5, c);
                p.dot(5.8, 6.3, 2.2, c);
                p.polyline(&[(4.0, 11.0), (7.0, 8.0), (9.3, 10.3), (10.8, 8.8), (12.0, 10.0)], c);
            }
            FileKind::Audio => {
                p.dot(5.5, 11.8, 4.4, c);
                let w = p.stroke_units();
                p.rect(7.7 - w, 3.0, w, 8.8, 0.0, c, 1.0);
                p.line((7.7 - w / 2.0, 3.3), (11.5, 5.6), c);
            }
            FileKind::Rust => {
                p.ring(8.0, 8.0, 8.4, c);
                for (x, y) in [(8.0, 2.4), (8.0, 13.6), (2.4, 8.0), (13.6, 8.0)] {
                    p.dot(x, y, 2.6, c);
                }
                for (x, y) in [(4.0, 4.0), (12.0, 4.0), (4.0, 12.0), (12.0, 12.0)] {
                    p.dot(x, y, 2.4, c);
                }
                p.dot(8.0, 8.0, 2.6, c);
            }
            FileKind::Folder { open } => {
                p.rect(2.0, 3.0, 5.5, 3.0, 1.0, c, 1.0);
                if open {
                    p.outline(2.0, 4.5, 12.0, 9.0, 1.5, c);
                    p.rect(2.0, 7.5, 12.0, 6.0, 1.5, c, 1.0);
                } else {
                    p.rect(2.0, 4.5, 12.0, 9.0, 1.5, c, 1.0);
                }
            }
        }
    }
}

/// A UI icon, drawn in one colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    ChevronRight,
    ChevronDown,
    /// Show / hide a sidebar (a bulleted list).
    SidebarToggle,
    /// `⋯`: more options.
    More,
    /// View mode Preview (an eye).
    ViewPreview,
    /// View mode Source (`</>`).
    ViewSource,
    /// View mode Integrated (`<•>`: source and preview mixed).
    ViewMixed,
    Copy,
    Search,
    Close,
}

impl Icon {
    /// Every icon, for tests and galleries.
    pub const ALL: [Icon; 10] = [
        Icon::ChevronRight,
        Icon::ChevronDown,
        Icon::SidebarToggle,
        Icon::More,
        Icon::ViewPreview,
        Icon::ViewSource,
        Icon::ViewMixed,
        Icon::Copy,
        Icon::Search,
        Icon::Close,
    ];

    /// Draw the icon into (a square centred in) `r` in `color`, under the text.
    pub fn paint(self, cx: &mut PaintCx, r: Rect, color: [u8; 3]) {
        let c = color;
        let mut p = Pen::new(cx, r);
        match self {
            Icon::ChevronRight => p.polyline(&[(6.0, 4.0), (10.0, 8.0), (6.0, 12.0)], c),
            Icon::ChevronDown => p.polyline(&[(4.0, 6.0), (8.0, 10.0), (12.0, 6.0)], c),
            Icon::SidebarToggle => {
                let w = p.stroke_units();
                for y in [4.0, 8.0, 12.0] {
                    p.dot(3.0, y, 2.0, c);
                    p.rect(6.0, y - w / 2.0, 8.0, w, 0.0, c, 1.0);
                }
            }
            Icon::More => {
                for x in [3.0, 8.0, 13.0] {
                    p.dot(x, 8.0, 2.2, c);
                }
            }
            Icon::ViewPreview => {
                p.polyline(&[(1.5, 8.0), (3.6, 5.6), (6.0, 4.3), (10.0, 4.3), (12.4, 5.6), (14.5, 8.0)], c);
                p.polyline(&[(1.5, 8.0), (3.6, 10.4), (6.0, 11.7), (10.0, 11.7), (12.4, 10.4), (14.5, 8.0)], c);
                p.dot(8.0, 8.0, 4.2, c);
            }
            Icon::ViewSource => p.code(c),
            Icon::ViewMixed => {
                p.polyline(&[(5.0, 4.5), (1.5, 8.0), (5.0, 11.5)], c);
                p.polyline(&[(11.0, 4.5), (14.5, 8.0), (11.0, 11.5)], c);
                p.dot(8.0, 8.0, 3.6, c);
            }
            Icon::Copy => {
                // The back sheet shows only where the front one doesn't cover it.
                let w = p.stroke_units();
                p.rect(2.0, 2.0, 9.0, w, 0.0, c, 1.0);
                p.rect(2.0, 2.0, w, 9.0, 0.0, c, 1.0);
                p.rect(11.0 - w, 2.0, w, 3.0, 0.0, c, 1.0);
                p.rect(2.0, 11.0 - w, 3.0, w, 0.0, c, 1.0);
                p.outline(5.0, 5.0, 9.0, 9.0, 1.5, c);
            }
            Icon::Search => {
                p.ring(7.0, 7.0, 9.0, c);
                p.line((10.4, 10.4), (13.8, 13.8), c);
            }
            Icon::Close => {
                p.line((4.5, 4.5), (11.5, 11.5), c);
                p.line((11.5, 4.5), (4.5, 11.5), c);
            }
        }
    }
}

/// A colour moved `t` of the way to black.
fn shade(c: [u8; 3], t: f32) -> [u8; 3] {
    c.map(|v| (v as f32 * (1.0 - t)).round() as u8)
}

/// A colour moved `t` of the way to white.
fn tint(c: [u8; 3], t: f32) -> [u8; 3] {
    c.map(|v| (v as f32 + (255.0 - v as f32) * t).round() as u8)
}

/// Draws on an icon's 16-unit grid.
struct Pen<'c, 'a> {
    cx: &'c mut PaintCx<'a>,
    x: f32,
    y: f32,
    /// Pixels per grid unit.
    u: f32,
    /// Stroke width in pixels.
    stroke: f32,
}

impl<'c, 'a> Pen<'c, 'a> {
    fn new(cx: &'c mut PaintCx<'a>, r: Rect) -> Self {
        let side = r.w.min(r.h).max(1.0).round();
        let x = (r.x + (r.w - side) / 2.0).round();
        let y = (r.y + (r.h - side) / 2.0).round();
        Pen {
            cx,
            x,
            y,
            u: side / 16.0,
            stroke: (side / 12.0).round().max(1.0),
        }
    }

    /// The stroke width in grid units.
    fn stroke_units(&self) -> f32 {
        self.stroke / self.u
    }

    fn px(&self, gx: f32, gy: f32) -> (f32, f32) {
        (self.x + gx * self.u, self.y + gy * self.u)
    }

    /// A filled rect, snapped to whole pixels (at least one wide and high).
    #[allow(clippy::too_many_arguments)]
    fn rect(&mut self, gx: f32, gy: f32, gw: f32, gh: f32, radius: f32, c: [u8; 3], alpha: f32) {
        let r = self.snapped(gx, gy, gw, gh);
        if radius > 0.0 {
            self.cx.round_rect(r, radius * self.u, c, alpha, None);
        } else {
            self.cx.fill_alpha(r, c, alpha);
        }
    }

    /// A stroke-wide outline, snapped to whole pixels.
    fn outline(&mut self, gx: f32, gy: f32, gw: f32, gh: f32, radius: f32, c: [u8; 3]) {
        let r = self.snapped(gx, gy, gw, gh);
        self.cx.round_rect(r, radius * self.u, c, 0.0, Some((self.stroke, c, 1.0)));
    }

    /// A stroke-wide circle outline `d` units across, centred on `(gx, gy)`.
    fn ring(&mut self, gx: f32, gy: f32, d: f32, c: [u8; 3]) {
        let (x, y) = self.px(gx, gy);
        let d = d * self.u;
        let r = Rect { x: x - d / 2.0, y: y - d / 2.0, w: d, h: d };
        self.cx.round_rect(r, d / 2.0, c, 0.0, Some((self.stroke, c, 1.0)));
    }

    /// A filled circle `d` units across, centred on `(gx, gy)`.
    fn dot(&mut self, gx: f32, gy: f32, d: f32, c: [u8; 3]) {
        let (x, y) = self.px(gx, gy);
        let d = d * self.u;
        let r = Rect { x: x - d / 2.0, y: y - d / 2.0, w: d, h: d };
        self.cx.round_rect(r, d / 2.0, c, 1.0, None);
    }

    /// A stroke from `a` to `b` with round ends.
    fn line(&mut self, a: (f32, f32), b: (f32, f32), c: [u8; 3]) {
        let (a, b) = (self.px(a.0, a.1), self.px(b.0, b.1));
        self.cx.line(a, b, self.stroke, c, 1.0);
    }

    /// Strokes through `pts`. Opaque, so the joints' overlap doesn't show.
    fn polyline(&mut self, pts: &[(f32, f32)], c: [u8; 3]) {
        for w in pts.windows(2) {
            self.line(w[0], w[1], c);
        }
    }

    /// A filled diamond centred on `(gx, gy)`, `half` units from centre to
    /// tip: a square on its corner, drawn as one flat-ended bar along a
    /// diagonal.
    fn diamond(&mut self, gx: f32, gy: f32, half: f32, c: [u8; 3]) {
        let (x, y) = self.px(gx, gy);
        let h = half * self.u;
        // The square's side is h * sqrt(2); its axis runs between the
        // midpoints of two opposite edges, (x -+ h/2, y -+ h/2).
        let side = h * std::f32::consts::SQRT_2;
        self.cx.bar((x - h / 2.0, y - h / 2.0), (x + h / 2.0, y + h / 2.0), side, c, 1.0);
    }

    /// `</>`.
    fn code(&mut self, c: [u8; 3]) {
        self.polyline(&[(5.0, 4.5), (1.5, 8.0), (5.0, 11.5)], c);
        self.polyline(&[(11.0, 4.5), (14.5, 8.0), (11.0, 11.5)], c);
        self.line((9.5, 3.5), (6.5, 12.5), c);
    }

    /// A page with its top-right corner folded.
    fn doc(&mut self, c: [u8; 3]) {
        self.rect(3.0, 1.5, 10.0, 13.0, 1.5, c, 1.0);
        self.rect(9.5, 1.5, 3.5, 3.5, 1.0, tint(c, 0.45), 1.0);
    }

    fn snapped(&self, gx: f32, gy: f32, gw: f32, gh: f32) -> Rect {
        let (x0, y0) = self.px(gx, gy);
        let (x1, y1) = self.px(gx + gw, gy + gh);
        let (x0, y0) = (x0.round(), y0.round());
        Rect {
            x: x0,
            y: y0,
            w: (x1.round() - x0).max(1.0),
            h: (y1.round() - y0).max(1.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ghostrealm_core::{Rect, SurfaceId};

    use super::{FileKind, Icon};
    use crate::plugin::paint::QuadInstance;
    use crate::plugin::testing::{Harness, UI};
    use crate::plugin::{Layer, PaintCx};

    /// Paint `f` with a fresh frame; returns the under-text quads and the
    /// number of text items.
    fn paint(f: impl FnOnce(&mut PaintCx)) -> (Vec<QuadInstance>, usize) {
        let mut h = Harness::new();
        h.text.begin_frame(0, glyphon::Metrics::new(14.0, UI.cell_h), UI.cell_h, std::time::Duration::from_millis(50));
        {
            let mut cx = PaintCx::new(
                &h.cfg, &h.chrome, UI, (-1.0, -1.0), &mut h.text, &mut h.frame, Layer::Base, SurfaceId(1),
            );
            f(&mut cx);
        }
        (h.frame.bg.clone(), h.frame.text.len() + h.frame.top.len())
    }

    /// A quad's rect in physical px on the harness's 800x600 frame.
    fn px_rect(q: &QuadInstance) -> Rect {
        let (sw, sh) = (800.0, 600.0);
        Rect {
            x: (q.pos[0] + 1.0) / 2.0 * sw,
            y: (1.0 - q.pos[1]) / 2.0 * sh,
            w: q.px[0],
            h: q.px[1],
        }
    }

    const ROW: Rect = Rect { x: 100.0, y: 50.0, w: 16.0, h: 16.0 };

    /// Every quad lies inside the icon's square (strokes may round a pixel
    /// over).
    fn assert_inside(quads: &[QuadInstance], what: &str) {
        assert!(!quads.is_empty(), "{what} drew nothing");
        for q in quads {
            let r = px_rect(q);
            assert!(
                r.x >= ROW.x - 1.0 && r.y >= ROW.y - 1.0 && r.x + r.w <= ROW.x + ROW.w + 1.0 && r.y + r.h <= ROW.y + ROW.h + 1.0,
                "{what} quad {r:?} outside {ROW:?}"
            );
        }
    }

    fn all_kinds() -> [FileKind; 10] {
        [
            FileKind::Markdown,
            FileKind::Text,
            FileKind::Code,
            FileKind::Json,
            FileKind::Git,
            FileKind::Image,
            FileKind::Audio,
            FileKind::Rust,
            FileKind::Folder { open: false },
            FileKind::Folder { open: true },
        ]
    }

    #[test]
    fn every_file_icon_draws_quads_only_inside_its_square_in_its_colour() {
        for kind in all_kinds() {
            let (quads, other) = paint(|cx| kind.paint(cx, ROW));
            assert_inside(&quads, &format!("{kind:?}"));
            assert_eq!(other, 0, "{kind:?} draws only under-text quads, no glyphs");
            let want = crate::plugin::paint::rect_quad(ROW, 800.0, 600.0, kind.color(), 1.0).color;
            assert!(quads.iter().any(|q| q.color == want), "{kind:?} uses its colour");
        }
    }

    #[test]
    fn file_icons_differ_from_each_other() {
        let shapes: Vec<Vec<[f32; 4]>> = all_kinds()
            .iter()
            .map(|k| {
                let (quads, _) = paint(|cx| k.paint(cx, ROW));
                quads.iter().map(|q| { let r = px_rect(q); [r.x, r.y, r.w, r.h] }).collect()
            })
            .collect();
        for i in 0..shapes.len() {
            for j in i + 1..shapes.len() {
                assert_ne!(shapes[i], shapes[j], "{:?} and {:?} look alike", all_kinds()[i], all_kinds()[j]);
            }
        }
    }

    #[test]
    fn every_ui_icon_draws_inside_its_square_in_the_given_colour() {
        let c = [10, 200, 30];
        let want = crate::plugin::paint::rect_quad(ROW, 800.0, 600.0, c, 1.0).color;
        for icon in Icon::ALL {
            let (quads, other) = paint(|cx| icon.paint(cx, ROW, c));
            assert_inside(&quads, &format!("{icon:?}"));
            assert_eq!(other, 0, "{icon:?} draws only under-text quads");
            assert!(
                quads.iter().all(|q| q.color[..3] == want[..3]
                    && (q.radius < 0.0 || q.border_w == 0.0 || q.border_color == want)),
                "{icon:?} is drawn in one colour"
            );
        }
    }

    /// An icon is laid out in the largest square that fits, centred, on
    /// whole pixels, so a wide row centres it and it stays crisp.
    #[test]
    fn icons_centre_in_a_wide_rect_on_whole_pixels() {
        let wide = Rect { x: 100.0, y: 50.0, w: 40.0, h: 16.0 };
        let (quads, _) = paint(|cx| FileKind::Text.paint(cx, wide));
        let page = px_rect(&quads[0]);
        // Within float error of the NDC round trip.
        let whole = |v: f32| (v - v.round()).abs() < 1e-3;
        assert!(whole(page.x) && whole(page.y) && whole(page.w) && whole(page.h), "snapped to pixels: {page:?}");
        assert!(page.x >= 112.0 && page.x + page.w <= 128.0, "centred in the row: {page:?}");
    }

    #[test]
    fn file_kinds_follow_name_and_extension() {
        for (path, kind) in [
            ("README.md", FileKind::Markdown),
            ("notes.MARKDOWN", FileKind::Markdown),
            ("Cargo.lock", FileKind::Text),
            ("CACHEDIR.TAG", FileKind::Text),
            ("notes.txt", FileKind::Text),
            ("LICENSE", FileKind::Text),
            ("Cargo.toml", FileKind::Code),
            ("main.c", FileKind::Code),
            ("app.tsx", FileKind::Code),
            ("build.py", FileKind::Code),
            ("Makefile", FileKind::Code),
            ("Dockerfile", FileKind::Code),
            ("Dockerfile.dev", FileKind::Code),
            ("api.proto", FileKind::Code),
            ("run.sh", FileKind::Code),
            ("ci.yml", FileKind::Code),
            (".gitlab-ci.yml", FileKind::Code),
            (".gitmodules", FileKind::Git),
            (".rustc_info.json", FileKind::Json),
            ("tsconfig.jsonc", FileKind::Json),
            (".gitignore", FileKind::Git),
            (".gitattributes", FileKind::Git),
            ("logo.PNG", FileKind::Image),
            ("icon.svg", FileKind::Image),
            ("song.mp3", FileKind::Audio),
            ("take.wav", FileKind::Audio),
            ("lib.rs", FileKind::Rust),
            ("dir/sub/mod.rs", FileKind::Rust),
        ] {
            assert_eq!(FileKind::of(Path::new(path)), kind, "{path}");
        }
    }
}
