//! The text editor plugin: a minimal editable buffer with line numbers,
//! soft-wrap, selection, and TOML highlighting. It claims any file path, so it
//! is the fallback when no more specific plugin opens a file.

pub mod buffer;
pub mod toml;
pub mod wrap;

use std::any::Any;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use ghostrealm_core::{
    ArgKind, ArgSpec, CmdError, CmdOutcome, CommandMeta, Config, LineNumbers, Rect, Registry,
};
use ghostrealm_terminal::{Key, KeyPress};

use crate::app_state::{AppState, Pick};
use crate::plugin::{
    hover_box, rect_contains, theme, EventCx, Font, MouseEvent, OpenCx, PaintCx, Plugin, Request,
    View,
};

pub use buffer::{EditorBuffer, Motion};
use toml::toml_row_spans;
use wrap::wrap_line;

pub const ID: &str = "editor";

/// Editor text colour.
pub const EDITOR_FG: [u8; 3] = [220, 220, 230];
/// Editor background (slightly distinct from a terminal).
const EDITOR_BG: [u8; 3] = [26, 26, 32];
/// Line-number gutter: dim, brighter on the cursor's line.
const EDITOR_GUTTER: [u8; 3] = [110, 110, 125];
const EDITOR_GUTTER_CUR: [u8; 3] = [190, 190, 205];
/// Active-line highlight fill behind the cursor row.
const EDITOR_CURSOR_LINE: [u8; 3] = [255, 255, 255];
const EDITOR_CURSOR_LINE_ALPHA: f32 = 0.05;
/// Spaces a Tab inserts.
const TAB_WIDTH: usize = 4;

/// The ribbon's soft-wrap toggle (see [`View::button`]).
const BTN_WRAP: u32 = 0;

pub struct EditorPlugin;

impl Plugin for EditorPlugin {
    fn id(&self) -> &'static str {
        ID
    }

    fn title(&self) -> &'static str {
        "Editor"
    }

    fn open(&self, _cx: &OpenCx) -> Result<Box<dyn View>> {
        Ok(Box::new(EditorView::new(EditorBuffer::scratch())))
    }

    fn opens_path(&self, _path: &Path) -> bool {
        true
    }

    fn open_path(&self, path: &Path, _cx: &OpenCx) -> Result<Box<dyn View>> {
        Ok(Box::new(EditorView::new(EditorBuffer::open(path.to_path_buf()))))
    }

    fn register_commands(&self, r: &mut Registry<AppState>) {
        let failed = |e: anyhow::Error| CmdError::Failed(format!("{e:#}"));
        r.register(
            CommandMeta::new("editor.new", "New Editor", "Open an empty text editor in the focused pane"),
            Box::new(move |s: &mut AppState, _| {
                s.open_plugin_in_focused(ID).map_err(failed)?;
                Ok(CmdOutcome::ok())
            }),
        );
        r.register(
            CommandMeta::new(
                "editor.open",
                "Open File in Editor",
                "Open a file in a text editor in the focused pane",
            )
            .arg(ArgSpec::optional("path", ArgKind::Str, "the file to open (chosen in a picker unless given)")),
            Box::new(move |s: &mut AppState, a| {
                if a.get("path").is_none() {
                    s.open_picker(Pick::OpenWith(ID));
                    return Ok(CmdOutcome::ok());
                }
                let path = PathBuf::from(a.get_str("path")?);
                s.open_path_with_in_focused(ID, &path).map_err(failed)?;
                Ok(CmdOutcome::ok())
            }),
        );
        r.register(
            CommandMeta::new(
                "editor.save_as",
                "Save As…",
                "Save the focused editor to a file chosen in a picker",
            ),
            Box::new(move |s: &mut AppState, _| {
                let sid = s
                    .focused_surface()
                    .filter(|sid| s.view(*sid).is_some_and(|v| v.plugin() == ID))
                    .ok_or_else(|| CmdError::Failed("no editor is focused".into()))?;
                s.open_save_as(sid);
                Ok(CmdOutcome::ok())
            }),
        );
    }
}

/// Where the last paint put the text, for mapping the mouse to (line, col).
#[derive(Clone, Copy)]
struct Layout {
    /// The text area including the gutter (below any ribbon).
    body: Rect,
    /// Left edge of the text (right of the gutter).
    text_x: f32,
    /// Columns per visual row when soft-wrapping, else `usize::MAX`.
    wrap_cols: usize,
}

pub struct EditorView {
    buf: EditorBuffer,
    /// This view's soft-wrap override; `None` follows `[editor] soft_wrap`.
    soft_wrap: Option<bool>,
    /// Sub-line wheel remainder (px) carried between events so slow scrolls
    /// accumulate instead of rounding away.
    scroll_accum: f32,
    /// The buffer's edit count when last looked at, and when an edit was last
    /// seen (drives the idle autosave).
    seen_edits: u64,
    last_edit_at: Option<Instant>,
    layout: Layout,
}

impl EditorView {
    pub fn new(buf: EditorBuffer) -> Self {
        EditorView {
            seen_edits: buf.edits,
            buf,
            soft_wrap: None,
            scroll_accum: 0.0,
            last_edit_at: None,
            layout: Layout {
                body: Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
                text_x: 0.0,
                wrap_cols: usize::MAX,
            },
        }
    }

    pub fn buffer(&self) -> &EditorBuffer {
        &self.buf
    }

    pub fn buffer_mut(&mut self) -> &mut EditorBuffer {
        &mut self.buf
    }

    fn wrap_on(&self, cfg: &Config) -> bool {
        self.soft_wrap.unwrap_or(cfg.editor.soft_wrap)
    }

    /// Start the idle-autosave clock if the buffer changed since last looked.
    fn note_edits(&mut self) {
        if self.buf.edits != self.seen_edits {
            self.seen_edits = self.buf.edits;
            if self.buf.modified {
                self.last_edit_at = Some(Instant::now());
            }
        }
    }

    /// Save to the backing file, reporting it to the app; without one, ask
    /// where to save.
    fn save(&mut self, cx: &mut EventCx) {
        let Some(path) = self.buf.path.clone() else {
            cx.request(Request::SaveAs);
            return;
        };
        if let Ok(true) = self.buf.save() {
            cx.request(Request::Saved(path));
        }
        self.last_edit_at = None;
    }

    /// The `(line, col)` under a point, from the last paint's layout (wrap- and
    /// scroll-aware); a point below the text lands at the end of the last line.
    fn pos_at(&self, x: f32, y: f32, cell_w: f32, cell_h: f32) -> Option<(usize, usize)> {
        let l = self.layout;
        if !rect_contains(l.body, x, y) {
            return None;
        }
        let e = &self.buf;
        let row_off = ((y - l.body.y) / cell_h).floor().max(0.0) as usize;
        let col_off = ((x - l.text_x) / cell_w).round().max(0.0) as usize;
        // Walk visual rows from the scroll top to the clicked one, as paint does.
        let mut vidx = 0usize;
        for ln in e.scroll..e.lines.len() {
            if l.wrap_cols == usize::MAX {
                if vidx == row_off {
                    // Unwrapped rows are shifted left by hscroll.
                    let hs = e.hscroll;
                    return Some((ln, (hs + col_off).min(hs.max(e.line_len(ln)))));
                }
                vidx += 1;
            } else {
                for (text, start) in wrap_line(&e.lines[ln], l.wrap_cols) {
                    if vidx == row_off {
                        return Some((ln, (start + col_off).min(start + text.chars().count())));
                    }
                    vidx += 1;
                }
            }
        }
        let last = e.lines.len().saturating_sub(1);
        Some((last, e.line_len(last)))
    }

    /// The filename ribbon a single-tab editor shows in place of a tab: the name
    /// (italic when unsaved), size/line-count metrics, and the soft-wrap toggle.
    fn paint_ribbon(&mut self, cx: &mut PaintCx, r: Rect, wrap: bool) {
        let (cw, ch) = (cx.ui.cell_w, cx.ui.cell_h);
        let pad = 8.0 * cx.ui.scale;
        cx.fill(r, cx.chrome.sidebar);
        let top = r.y + (r.h - ch) * 0.5;
        // Soft-wrap toggle, pinned to the right end.
        let wb = Rect { x: r.x + r.w - ch - pad, y: r.y, w: ch, h: r.h };
        if cx.button(wb, BTN_WRAP) {
            cx.highlight(hover_box(wb, ch));
        }
        let wb_color = if wrap { cx.chrome.accent } else { theme::GLYPH };
        cx.label("\u{21a9}", Font::MONO, wb.x + (wb.w - cw) * 0.5, top, wb, wb_color);
        // Metrics, right-aligned before the toggle.
        let e = &self.buf;
        let bytes = e.lines.iter().map(|l| l.len()).sum::<usize>() + e.lines.len().saturating_sub(1);
        let meta = format!("{bytes} B   \u{b7}   {} lines", e.lines.len());
        let meta_left = wb.x - pad - meta.chars().count() as f32 * cw;
        let meta_clip = Rect { x: meta_left, y: r.y, w: (r.x + r.w - meta_left).max(0.0), h: r.h };
        cx.label(&meta, Font::SANS, meta_left, top, meta_clip, theme::TEXT_DIM);
        // Filename, left-aligned, clipped before the metrics.
        let name_clip = Rect { x: r.x, y: r.y, w: (meta_left - r.x).max(0.0), h: r.h };
        let font = Font::SANS.italic(e.modified);
        cx.label(&e.title(), font, r.x + pad, top, name_clip, theme::TEXT);
    }
}

/// Width (physical px) of the line-number gutter for `total_lines` lines: the
/// digits plus a column of padding each side. Zero when line numbers are off.
fn gutter_width(cfg: &Config, total_lines: usize, cell_w: f32) -> f32 {
    if cfg.editor.line_numbers == LineNumbers::Off {
        return 0.0;
    }
    let digits = ((total_lines.max(1) as f64).log10().floor() as usize + 1).max(2);
    (digits as f32 + 2.0) * cell_w
}

impl View for EditorView {
    fn plugin(&self) -> &'static str {
        ID
    }

    fn title(&self) -> Option<String> {
        Some(self.buf.title())
    }

    fn modified(&self) -> bool {
        self.buf.modified
    }

    fn paint(&mut self, cx: &mut PaintCx, rect: Rect) {
        let (cw, ch, scale) = (cx.ui.cell_w, cx.ui.cell_h, cx.ui.scale);
        let wrap = self.wrap_on(cx.cfg);
        // Without a tab strip, the filename gets a ribbon of its own.
        let mut body = rect;
        if !cx.tab_strip {
            let ribbon_h = ch + 6.0 * scale;
            self.paint_ribbon(cx, Rect { h: ribbon_h, ..rect }, wrap);
            body = Rect { y: rect.y + ribbon_h, h: (rect.h - ribbon_h).max(1.0), ..rect };
        }

        let rows_vis = (body.h / ch).floor().max(1.0) as usize;
        let total_lines = self.buf.lines.len();
        let gutter_w = gutter_width(cx.cfg, total_lines, cw);
        let text_x = body.x + gutter_w;
        let text_w = (body.w - gutter_w).max(1.0);
        let text_cols = ((text_w / cw).floor() as usize).max(1);
        let wrap_cols = if wrap { text_cols } else { usize::MAX };
        self.layout = Layout { body, text_x, wrap_cols };
        // Follow the cursor only when it moved (so manual scrolling sticks);
        // reset horizontal scroll under wrap; keep vertical scroll in bounds.
        self.buf.follow_cursor(rows_vis, if wrap { None } else { Some(text_cols) });
        if wrap {
            self.buf.hscroll = 0;
        }
        self.buf.clamp_scroll_bounds();

        let e = &self.buf;
        let (scroll, cursor, sel) = (e.scroll, e.cursor, e.selection());
        // Unwrapped rows hold their whole line, shaped once, drawn shifted left
        // by hscroll and clipped — moving the viewport rather than reshaping.
        let hscroll_px = e.hscroll as f32 * cw;
        let highlight_toml = e
            .path
            .as_ref()
            .and_then(|p| p.extension())
            .and_then(|x| x.to_str())
            .is_some_and(|x| x.eq_ignore_ascii_case("toml"));
        cx.fill(body, EDITOR_BG);

        // Visible visual rows: (logical line, text, start col, y).
        let bottom = body.y + body.h;
        let mut vis: Vec<(usize, String, usize, f32)> = Vec::new();
        let mut vy = body.y;
        'lines: for ln in scroll..(scroll + rows_vis).min(e.lines.len()) {
            let line = &e.lines[ln];
            let rows = if wrap {
                wrap_line(line, wrap_cols)
            } else {
                vec![(line.clone(), 0)]
            };
            for (text, start) in rows {
                if vy >= bottom {
                    break 'lines;
                }
                vis.push((ln, text, start, vy));
                vy += ch;
            }
        }

        if cx.cfg.editor.cursor_line {
            for (ln, _, _, y) in &vis {
                if *ln == cursor.0 {
                    let r = Rect { x: body.x, y: *y, w: body.w, h: ch };
                    cx.fill_alpha(r, EDITOR_CURSOR_LINE, EDITOR_CURSOR_LINE_ALPHA);
                }
            }
        }
        // Selection, intersected with each row and clipped to the text area so it
        // never paints the gutter or a neighbouring pane.
        if let Some(((sr, sc), (er, ec))) = sel {
            for (ln, text, start, y) in &vis {
                let ln = *ln;
                if ln < sr || ln > er {
                    continue;
                }
                let first = if ln == sr { sc } else { 0 }.max(*start);
                let last = if ln == er { ec } else { usize::MAX }.min(start + text.chars().count());
                if last > first {
                    let x0 = (text_x + (first - start) as f32 * cw - hscroll_px).max(text_x);
                    let x1 = (text_x + (last - start) as f32 * cw - hscroll_px).min(body.x + body.w);
                    if x1 > x0 {
                        let r = Rect { x: x0, y: *y, w: x1 - x0, h: ch };
                        cx.fill_alpha(r, cx.chrome.accent, 0.35);
                    }
                }
            }
        }
        // Line numbers, on each logical line's first visual row.
        if gutter_w > 0.0 {
            let gutter = Rect { x: body.x, y: body.y, w: gutter_w, h: body.h };
            for (ln, _, start, y) in &vis {
                if *start != 0 {
                    continue;
                }
                let is_cur = *ln == cursor.0;
                let num = match cx.cfg.editor.line_numbers {
                    LineNumbers::Relative if !is_cur => (cursor.0 as isize - *ln as isize).unsigned_abs(),
                    _ => ln + 1,
                };
                let s = num.to_string();
                let color = if is_cur { EDITOR_GUTTER_CUR } else { EDITOR_GUTTER };
                let left = text_x - cw - s.chars().count() as f32 * cw;
                cx.row(&[(s, color)], left, *y, gutter_w, gutter, color);
            }
        }
        // Text, one cache-keyed row per visual row (wrapped sub-rows stay
        // cache-friendly), shaped within the frame's budget.
        let text_clip = Rect { x: text_x, y: body.y, w: body.x + body.w - text_x, h: body.h };
        for (ln, text, start, y) in &vis {
            let spans: Vec<(String, [u8; 3])> = if highlight_toml {
                toml_row_spans(&self.buf.lines[*ln], *start, text.chars().count())
            } else {
                vec![(text.clone(), EDITOR_FG)]
            };
            let key = cx.row_key(spans.iter().map(|(s, c)| (s.as_str(), *c)));
            // Wide enough for the whole row, so layout is hscroll-independent.
            let shape_w = (text.chars().count() as f32 * cw).max(text_w);
            cx.budgeted_row(key, || spans, text_x - hscroll_px, *y, shape_w, text_clip, EDITOR_FG);
        }
        // Cursor bar on the cursor line's last visual row that its column reaches.
        if let Some((_, _, start, y)) =
            vis.iter().rev().find(|(ln, _, start, _)| *ln == cursor.0 && *start <= cursor.1)
        {
            let x = text_x + (cursor.1 - start) as f32 * cw - hscroll_px;
            if x >= text_x && x < body.x + body.w {
                cx.fill_top(Rect { x, y: *y, w: 2.0 * scale, h: ch }, cx.chrome.accent, 0.9);
            }
        }
    }

    fn key(&mut self, cx: &mut EventCx, k: &KeyPress) -> bool {
        let used = if k.mods.super_ {
            self.cmd_key(cx, k)
        } else {
            self.plain_key(k)
        };
        if used {
            self.note_edits();
            cx.redraw();
        }
        used
    }

    fn mouse(&mut self, cx: &mut EventCx, event: &MouseEvent) {
        let (cw, ch) = (cx.ui.cell_w, cx.ui.cell_h);
        match *event {
            // A press moves the cursor and anchors a potential selection; one only
            // shows once the cursor is dragged away.
            MouseEvent::Down { pos: (x, y), .. } => {
                if let Some(p) = self.pos_at(x, y, cw, ch) {
                    self.buf.cursor = p;
                    self.buf.anchor = Some(p);
                    cx.redraw();
                }
            }
            MouseEvent::Drag { pos: (x, y) } => {
                if let Some(p) = self.pos_at(x, y, cw, ch) {
                    if self.buf.cursor != p {
                        self.buf.cursor = p;
                        cx.redraw();
                    }
                }
            }
            // A plain click leaves no selection; a drag keeps it (for Cmd+C).
            MouseEvent::Up { dragged, .. } => {
                if !dragged {
                    self.buf.anchor = None;
                    cx.redraw();
                }
            }
            MouseEvent::Move { .. } => {}
        }
    }

    fn scroll(&mut self, cx: &mut EventCx, _pos: (f32, f32), dx: f32, dy: f32) {
        let (cw, ch) = (cx.ui.cell_w, cx.ui.cell_h);
        self.scroll_accum += dy;
        let lines = (self.scroll_accum / ch).trunc() as i32;
        self.scroll_accum -= lines as f32 * ch;
        let e = &mut self.buf;
        let mut changed = false;
        if lines != 0 {
            let max = e.lines.len().saturating_sub(1);
            let next = ((e.scroll as isize - lines as isize).max(0) as usize).min(max);
            changed |= next != e.scroll;
            e.scroll = next;
        }
        // Horizontal scroll only matters when lines aren't wrapped.
        let wrap = self.soft_wrap.unwrap_or(cx.cfg.editor.soft_wrap);
        let cols = (dx / cw).round() as isize;
        if !wrap && cols != 0 {
            let widest = e.lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
            let next = ((e.hscroll as isize - cols).max(0) as usize).min(widest);
            changed |= next != e.hscroll;
            e.hscroll = next;
        }
        if changed {
            cx.request_frame();
        }
    }

    fn button(&mut self, cx: &mut EventCx, id: u32) {
        if id == BTN_WRAP {
            self.soft_wrap = Some(!self.wrap_on(cx.cfg));
            cx.redraw();
        }
    }

    fn focus_changed(&mut self, cx: &mut EventCx, focused: bool) {
        if !focused && cx.cfg.editor.autosave_on_unfocus {
            if let Some(path) = self.autosave() {
                cx.request(Request::Saved(path));
            }
        }
    }

    fn deadline(&self, cfg: &Config) -> Option<Instant> {
        let after = cfg.editor.autosave_after;
        if after == 0 {
            return None;
        }
        self.last_edit_at.map(|t| t + Duration::from_secs(after as u64))
    }

    fn tick(&mut self, cx: &mut EventCx, now: Instant) {
        if self.deadline(cx.cfg).is_some_and(|d| now >= d) {
            self.last_edit_at = None;
            if let Some(path) = self.autosave() {
                cx.request(Request::Saved(path));
            }
            cx.redraw();
        }
    }

    fn autosave(&mut self) -> Option<PathBuf> {
        if !self.buf.modified || self.buf.path.is_none() {
            return None;
        }
        self.buf.save().ok().filter(|saved| *saved)?;
        self.last_edit_at = None;
        self.buf.path.clone()
    }

    fn path(&self) -> Option<PathBuf> {
        self.buf.path.clone()
    }

    /// Creates missing parent directories, so a typed `sub/name` works.
    fn save_as(&mut self, path: PathBuf) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        self.buf.path = Some(path);
        self.buf.save()?;
        self.last_edit_at = None;
        Ok(())
    }

    /// Agent input is typed at the cursor.
    fn write_input(&mut self, bytes: &[u8]) -> bool {
        self.buf.insert_str(&String::from_utf8_lossy(bytes));
        self.note_edits();
        true
    }

    fn text(&mut self) -> Option<String> {
        Some(self.buf.lines.join("\n"))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl EditorView {
    /// Keys without Cmd: motions (Shift extends the selection) and typing.
    fn plain_key(&mut self, k: &KeyPress) -> bool {
        let e = &mut self.buf;
        let motion = match k.key {
            Key::Left => Some(Motion::Left),
            Key::Right => Some(Motion::Right),
            Key::Up => Some(Motion::Up),
            Key::Down => Some(Motion::Down),
            Key::Home => Some(Motion::Home),
            Key::End => Some(Motion::End),
            _ => None,
        };
        if let Some(m) = motion {
            if k.mods.shift {
                e.move_cursor_selecting(m);
            } else {
                e.move_cursor(m);
            }
            return true;
        }
        match k.key {
            Key::Enter => e.insert_newline(),
            Key::Backspace => e.backspace(),
            Key::Delete => e.delete_forward(),
            Key::Tab => e.insert_str(&" ".repeat(TAB_WIDTH)),
            // Ctrl chords aren't text.
            Key::Char(_) if k.mods.ctrl => return false,
            Key::Char(c) => {
                let typed = k.text.clone().unwrap_or_else(|| c.to_string());
                let typed: String = typed.chars().filter(|c| !c.is_control()).collect();
                if typed.is_empty() {
                    return false;
                }
                e.insert_str(&typed);
            }
            _ => return false,
        }
        true
    }

    /// Cmd chords the app didn't bind: clipboard (C/X/V), save (S; Shift+S saves
    /// as), and line / document navigation (arrows; Shift extends the selection).
    fn cmd_key(&mut self, cx: &mut EventCx, k: &KeyPress) -> bool {
        let selecting = k.mods.shift;
        let e = &mut self.buf;
        match k.key {
            Key::Char(c) => match c.to_ascii_lowercase() {
                'c' => {
                    if let Some(t) = e.selected_text() {
                        cx.set_clipboard_text(t);
                    }
                }
                'x' => {
                    if let Some(t) = e.selected_text() {
                        e.delete_selection();
                        cx.set_clipboard_text(t);
                    }
                }
                'v' => {
                    if let Some(t) = cx.clipboard_text() {
                        e.insert_str(&t);
                    }
                }
                's' if selecting => cx.request(Request::SaveAs),
                's' => self.save(cx),
                _ => return false,
            },
            Key::Left if selecting => e.move_cursor_selecting(Motion::Home),
            Key::Left => e.move_cursor(Motion::Home),
            Key::Right if selecting => e.move_cursor_selecting(Motion::End),
            Key::Right => e.move_cursor(Motion::End),
            Key::Up => e.move_document(false, selecting),
            Key::Down => e.move_document(true, selecting),
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::testing::{cmd, press, tmpdir, Harness, UI};

    const RECT: Rect = Rect { x: 0.0, y: 0.0, w: 400.0, h: 200.0 };

    fn view_with(text: &str) -> EditorView {
        let mut b = EditorBuffer::scratch();
        b.insert_str(text);
        b.cursor = (0, 0);
        b.anchor = None;
        EditorView::new(b)
    }

    /// Where column `col` of visual row `row` sits, painted with a tab strip (no
    /// ribbon) and the default gutter (2 digits + 2 pad = 4 cells).
    fn at(row: usize, col: usize) -> (f32, f32) {
        let text_x = 4.0 * UI.cell_w;
        (text_x + col as f32 * UI.cell_w, row as f32 * UI.cell_h + 1.0)
    }

    fn paint_in_strip(h: &mut Harness, v: &mut EditorView) {
        h.tab_strip = true;
        h.paint(v, RECT);
    }

    #[test]
    fn a_single_tab_editor_draws_its_ribbon_with_the_wrap_toggle() {
        let mut v = view_with("hello");
        let mut h = Harness::new();
        let frame = h.paint(&mut v, RECT);
        assert!(frame.view_buttons.iter().any(|(_, _, id)| *id == BTN_WRAP));
        paint_in_strip(&mut h, &mut v);
        assert!(h.frame.view_buttons.is_empty(), "under a tab strip, no ribbon");
    }

    #[test]
    fn click_places_the_cursor_and_drag_selects() {
        let mut v = view_with("hello world\nsecond");
        let mut h = Harness::new();
        paint_in_strip(&mut h, &mut v);
        h.mouse(&mut v, MouseEvent::Down { pos: at(1, 3), clicks: 1 });
        assert_eq!(v.buffer().cursor, (1, 3));
        h.mouse(&mut v, MouseEvent::Drag { pos: at(0, 5) });
        h.mouse(&mut v, MouseEvent::Up { pos: at(0, 5), dragged: true });
        assert_eq!(v.buffer().selected_text().as_deref(), Some(" world\nsec"));
        // A plain click clears it.
        h.mouse(&mut v, MouseEvent::Down { pos: at(0, 1), clicks: 1 });
        h.mouse(&mut v, MouseEvent::Up { pos: at(0, 1), dragged: false });
        assert_eq!(v.buffer().selection(), None);
    }

    #[test]
    fn clicks_map_through_soft_wrapped_rows() {
        // 400px wide, 4-cell gutter: 46 text columns per visual row.
        let long = "word ".repeat(20);
        let mut v = view_with(&long);
        let mut h = Harness::new();
        paint_in_strip(&mut h, &mut v);
        h.mouse(&mut v, MouseEvent::Down { pos: at(1, 2), clicks: 1 });
        let (line, col) = v.buffer().cursor;
        assert_eq!(line, 0, "the second visual row is still line 0");
        assert!(col > 40, "column continues past the wrap: {col}");
    }

    #[test]
    fn typing_edits_and_ctrl_chords_do_not_type() {
        let mut v = view_with("");
        let h = Harness::new();
        let (used, out) = h.key(&mut v, press(Key::Char('a')));
        assert!(used && out.redraw);
        let mut ctrl_b = press(Key::Char('b'));
        ctrl_b.mods.ctrl = true;
        let (used, _) = h.key(&mut v, ctrl_b);
        assert!(!used, "a ctrl chord is not text");
        h.key(&mut v, press(Key::Tab));
        assert_eq!(v.buffer().lines, vec![format!("a{}", " ".repeat(TAB_WIDTH))]);
    }

    #[test]
    fn cmd_s_saves_and_reports_the_file() {
        let path = tmpdir("editor-save").join("f.txt");
        let mut v = EditorView::new(EditorBuffer::open(path.clone()));
        let h = Harness::new();
        h.key(&mut v, press(Key::Char('x')));
        assert!(v.modified());
        let (used, out) = h.key(&mut v, cmd(Key::Char('s')));
        assert!(used);
        assert_eq!(out.requests, vec![Request::Saved(path.clone())]);
        assert!(!v.modified());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\n");
    }

    #[test]
    fn saving_a_scratch_buffer_asks_where() {
        let mut v = view_with("draft");
        let h = Harness::new();
        let (used, out) = h.key(&mut v, cmd(Key::Char('s')));
        assert!(used);
        assert_eq!(out.requests, vec![Request::SaveAs]);
    }

    #[test]
    fn cmd_shift_s_asks_where_even_with_a_file() {
        let path = tmpdir("editor-save-as").join("f.txt");
        let mut v = EditorView::new(EditorBuffer::open(path));
        let h = Harness::new();
        let mut k = cmd(Key::Char('S'));
        k.mods.shift = true;
        let (_, out) = h.key(&mut v, k);
        assert_eq!(out.requests, vec![Request::SaveAs]);
    }

    #[test]
    fn save_as_writes_and_adopts_the_file() {
        let path = tmpdir("editor-save-as").join("sub").join("new.txt");
        let mut v = view_with("hello");
        v.save_as(path.clone()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
        assert_eq!(v.path(), Some(path));
        assert!(!v.modified());
        assert_eq!(v.title().as_deref(), Some("new.txt"));
    }

    #[test]
    fn unbound_cmd_chords_are_not_used() {
        let mut v = view_with("abc");
        let h = Harness::new();
        let (used, _) = h.key(&mut v, cmd(Key::Char('q')));
        assert!(!used);
        assert_eq!(v.buffer().lines, vec!["abc".to_string()]);
    }

    #[test]
    fn idle_autosave_arms_on_edit_and_fires_after_the_delay() {
        let path = tmpdir("editor-idle").join("f.txt");
        let mut v = EditorView::new(EditorBuffer::open(path.clone()));
        let h = Harness::new();
        assert_eq!(v.deadline(&h.cfg), None, "nothing to save yet");
        h.key(&mut v, press(Key::Char('y')));
        let due = v.deadline(&h.cfg).expect("an edit arms the timer");
        let out = h.tick(&mut v, due - Duration::from_millis(1));
        assert!(out.requests.is_empty(), "not before the deadline");
        let out = h.tick(&mut v, due);
        assert_eq!(out.requests, vec![Request::Saved(path)]);
        assert_eq!(v.deadline(&h.cfg), None, "disarmed once saved");
    }

    #[test]
    fn losing_focus_autosaves() {
        let path = tmpdir("editor-blur").join("f.txt");
        let mut v = EditorView::new(EditorBuffer::open(path.clone()));
        let h = Harness::new();
        h.key(&mut v, press(Key::Char('z')));
        let out = h.focus(&mut v, false);
        assert_eq!(out.requests, vec![Request::Saved(path)]);
    }

    #[test]
    fn wheel_scrolls_by_whole_lines_carrying_the_remainder() {
        let text: String = (0..50).map(|i| format!("line {i}\n")).collect();
        let mut v = view_with(&text);
        let h = Harness::new();
        h.scroll(&mut v, (10.0, 10.0), -UI.cell_h * 0.6);
        assert_eq!(v.buffer().scroll, 0, "under a line: nothing yet");
        let out = h.scroll(&mut v, (10.0, 10.0), -UI.cell_h * 0.6);
        assert_eq!(v.buffer().scroll, 1, "the remainder carried over");
        assert!(out.frame);
    }

    #[test]
    fn the_wrap_button_toggles_this_view_only() {
        let mut v = view_with("x");
        let h = Harness::new();
        let before = v.wrap_on(&h.cfg);
        h.button(&mut v, BTN_WRAP);
        assert_eq!(v.wrap_on(&h.cfg), !before);
    }
}
