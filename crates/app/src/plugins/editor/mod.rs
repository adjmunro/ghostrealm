//! The text editor plugin: a minimal editable buffer with line numbers,
//! soft-wrap, selection, and syntax highlighting. It claims any file path, so it
//! is the fallback when no more specific plugin opens a file. A file with a
//! language service shows its diagnostics and hover docs.

pub mod buffer;
mod stats;
pub mod lang;
pub mod wrap;

use std::any::Any;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ghostrealm_core::config::{Kind, OptionSpec, SectionSpec};
use ghostrealm_core::highlight::{self, SpanCache, StateCache};
use ghostrealm_core::{
    ArgKind, ArgSpec, CmdError, CmdOutcome, CommandMeta, Config, LineNumbers, Rect, Registry,
};
use ghostrealm_lsp::Client;
use ghostrealm_terminal::{Key, KeyPress};

use crate::app_state::{AppState, Pick};
use crate::file_menu::{FileTarget, Kind as TargetKind, Origin};
use crate::syntax;
use crate::plugin::text_edit::{edit_for, Edit};
use crate::plugin::{
    hover_box, rect_contains, theme, EventCx, Font, MouseEvent, OpenCx, PaintCx, Plugin, Request,
    RowStyles, View, Waker,
};

pub use buffer::EditorBuffer;
use stats::Stats;
use lang::{paint_popup, Lang, Pos};
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

/// The ribbon's toggles (see [`View::button`]).
const BTN_WRAP: u32 = 0;
const BTN_AUTOSAVE: u32 = 1;
const BTN_STAT: u32 = 2;
const BTN_NAME: u32 = 3;
/// The idle-autosave delay (seconds) when autosave is switched on in the ribbon
/// but `[editor] autosave_after` is 0.
const AUTOSAVE_AFTER_DEFAULT: u32 = 15;

pub struct EditorPlugin;

impl Plugin for EditorPlugin {
    fn id(&self) -> &'static str {
        ID
    }

    fn title(&self) -> &'static str {
        "New File"
    }

    fn open(&self, cx: &OpenCx) -> Result<Box<dyn View>> {
        Ok(Box::new(EditorView::with_waker(EditorBuffer::scratch(), cx.waker.clone())))
    }

    fn opens_path(&self, _path: &Path) -> bool {
        true
    }

    fn open_path(&self, path: &Path, cx: &OpenCx) -> Result<Box<dyn View>> {
        let buf = EditorBuffer::open(path.to_path_buf())
            .with_context(|| format!("can't edit {}", path.display()))?;
        let mut view = EditorView::with_waker(buf, cx.waker.clone());
        if let Some((client, language_id)) =
            cx.languages.as_ref().and_then(|l| l.start(path, cx.waker.clone()))
        {
            view.attach(client, language_id);
        }
        Ok(Box::new(view))
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
                "editor.toggle_comment",
                "Toggle Comment",
                "Comment or uncomment the selected lines of the focused editor, per the file's language",
            ),
            Box::new(move |s: &mut AppState, _| {
                let editor = s
                    .focused_surface()
                    .and_then(|sid| s.view_mut(sid))
                    .and_then(|v| v.downcast_mut::<EditorView>())
                    .ok_or_else(|| CmdError::Failed("no editor is focused".into()))?;
                editor.toggle_comment().map_err(|e| CmdError::Failed(e.into()))?;
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

    fn config_sections(&self) -> Vec<SectionSpec> {
        const LINE_NUMBERS: &[(&str, &str)] = &[
            ("off", "No gutter."),
            ("absolute", "Every line shows its number."),
            ("relative", "Distance from the cursor's line; the cursor's line shows its number."),
        ];
        vec![SectionSpec::options(
            "editor",
            "The text editor.",
            vec![
                OptionSpec::new("line_numbers", Kind::Enum(LINE_NUMBERS), Some("\"absolute\""), "Line-number gutter."),
                OptionSpec::new("cursor_line", Kind::Bool, Some("true"), "Highlight the line the cursor is on."),
                OptionSpec::new(
                    "soft_wrap",
                    Kind::Bool,
                    Some("true"),
                    "Wrap long lines at the pane edge (toggle per pane with the ribbon button). \
                     Off clips them.",
                ),
                OptionSpec::new(
                    "autosave_on_unfocus",
                    Kind::Bool,
                    Some("true"),
                    "Save a modified file when its editor loses focus.",
                ),
                OptionSpec::new(
                    "autosave_after",
                    Kind::Int { min: 0, max: 3600 },
                    Some("15"),
                    "Save a modified file after this many seconds without edits (0 = off).",
                ),
            ],
        )]
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
    /// This view's autosave override (the ribbon toggle); `None` follows
    /// `[editor] autosave_after` / `autosave_on_unfocus`.
    autosave: Option<bool>,
    /// When the buffer was last written (any save).
    last_saved: Option<Instant>,
    /// The ribbon's statistic (click to cycle).
    stats: Stats,
    /// Sub-line wheel remainder (px) carried between events so slow scrolls
    /// accumulate instead of rounding away.
    scroll_accum: f32,
    /// The buffer's edit count when last looked at, and when an edit was last
    /// seen (drives the idle autosave).
    seen_edits: u64,
    last_edit_at: Option<Instant>,
    layout: Layout,
    /// The ribbon's file name and the ribbon itself, as last painted (none
    /// under a tab strip or without a file): they open the file menu.
    name_hit: Option<Rect>,
    ribbon: Option<Rect>,
    /// The file's language service, if it has one.
    lang: Option<Lang>,
    /// Where each line's syntax-highlight state starts.
    hl_states: StateCache,
    /// Highlight of the lines on screen, kept between frames.
    hl_spans: SpanCache,
}

impl EditorView {
    pub fn new(buf: EditorBuffer) -> Self {
        Self::with_waker(buf, None)
    }

    /// Like [`Self::new`], waking the UI with `waker` when background work
    /// (the ribbon's statistic) finishes.
    pub fn with_waker(buf: EditorBuffer, waker: Option<Waker>) -> Self {
        let mut stats = Stats::new(waker);
        stats.fetch_head(buf.path.clone());
        EditorView {
            hl_states: StateCache::default(),
            hl_spans: SpanCache::default(),
            stats,
            seen_edits: buf.edits,
            buf,
            soft_wrap: None,
            autosave: None,
            last_saved: None,
            scroll_accum: 0.0,
            last_edit_at: None,
            layout: Layout {
                body: Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
                text_x: 0.0,
                wrap_cols: usize::MAX,
            },
            lang: None,
            name_hit: None,
            ribbon: None,
        }
    }

    /// Connect the buffer to a language service (`client`, speaking
    /// `language_id`). The service runs until this view is dropped.
    pub fn attach(&mut self, client: Client, language_id: &str) {
        let uri = match &self.buf.path {
            Some(p) => ghostrealm_lsp::text::file_uri(p),
            None => "untitled:scratch".parse().expect("a valid URI"),
        };
        self.lang = Some(Lang::new(client, uri, language_id, &self.buf.lines, self.buf.edits));
    }

    pub fn buffer(&self) -> &EditorBuffer {
        &self.buf
    }

    pub fn buffer_mut(&mut self) -> &mut EditorBuffer {
        &mut self.buf
    }

    /// The file's language service, if it has one.
    pub fn lang(&self) -> Option<&Lang> {
        self.lang.as_ref()
    }

    fn wrap_on(&self, cfg: &Config) -> bool {
        self.soft_wrap.unwrap_or(cfg.editor.soft_wrap)
    }

    /// Whether this view autosaves at all: the toggle, else whether `[editor]`
    /// enables either trigger.
    fn autosave_on(&self, cfg: &Config) -> bool {
        self.autosave
            .unwrap_or(cfg.editor.autosave_after > 0 || cfg.editor.autosave_on_unfocus)
    }

    /// Idle seconds before an autosave: `[editor] autosave_after` (0 = never),
    /// unless the toggle overrides it.
    fn autosave_after(&self, cfg: &Config) -> Option<u32> {
        let after = cfg.editor.autosave_after;
        match self.autosave {
            Some(false) => None,
            Some(true) => Some(if after > 0 { after } else { AUTOSAVE_AFTER_DEFAULT }),
            None => (after > 0).then_some(after),
        }
    }

    /// Whether losing focus (the pane's or the app's) or quitting autosaves.
    fn autosave_on_unfocus(&self, cfg: &Config) -> bool {
        self.autosave.unwrap_or(cfg.editor.autosave_on_unfocus)
    }

    /// Write a modified, file-backed buffer. Returns the file written.
    fn save_if_modified(&mut self) -> Option<PathBuf> {
        if !self.buf.modified || self.buf.path.is_none() {
            return None;
        }
        self.buf.save().ok().filter(|saved| *saved)?;
        self.saved();
        self.buf.path.clone()
    }

    /// Note a write: disarm the idle timer, stamp the save time.
    fn saved(&mut self) {
        self.last_edit_at = None;
        self.last_saved = Some(Instant::now());
        self.stats.fetch_head(self.buf.path.clone());
    }

    /// The autosave toggle's tooltip: its state and the last save.
    fn autosave_tip(&self, cfg: &Config) -> String {
        if self.buf.path.is_none() {
            return "Autosave off (no file)".to_string();
        }
        let state = if self.autosave_on(cfg) { "on" } else { "off" };
        let last = match self.last_saved {
            Some(t) => format!("Last save: {}", ago(t.elapsed())),
            None => "Not saved yet".to_string(),
        };
        format!("Autosave {state}\n{last}")
    }

    /// When the idle autosave is due, if armed.
    fn autosave_deadline(&self, cfg: &Config) -> Option<Instant> {
        let after = self.autosave_after(cfg)?;
        self.last_edit_at.map(|t| t + Duration::from_secs(after as u64))
    }

    /// Start the idle-autosave clock if the buffer changed since last looked,
    /// and send the change to the language service.
    fn note_edits(&mut self) {
        if self.buf.edits != self.seen_edits {
            self.seen_edits = self.buf.edits;
            if self.buf.modified {
                self.last_edit_at = Some(Instant::now());
            }
        }
        if let Some(lang) = &mut self.lang {
            lang.sync(&self.buf.lines, self.buf.edits);
        }
    }

    /// Comment or uncomment the selected lines per the language of the first
    /// one ([`highlight::Highlighter::comment_on`]). Returns `Err` when that
    /// language has no comment syntax.
    pub fn toggle_comment(&mut self) -> Result<(), &'static str> {
        const NONE: &str = "this language has no comment syntax";
        let h = self.buf.path.as_deref().and_then(highlight::for_path).ok_or(NONE)?;
        let row = self.buf.selection().map_or(self.buf.cursor.0, |((r, _), _)| r);
        self.sync_hl_states();
        let state = self.hl_states.state_in(h, &self.buf.lines, self.buf.edits, row);
        let syntax = h.comment_on(&self.buf.lines[row], state);
        if syntax == highlight::CommentSyntax::default() {
            return Err(NONE);
        }
        if self.buf.toggle_comment(syntax) {
            self.note_edits();
        }
        Ok(())
    }

    /// Tell the highlight state cache which lines the edits since it last
    /// heard have changed.
    fn sync_hl_states(&mut self) {
        if let Some(row) = self.buf.dirty_from.take() {
            self.hl_states.dirty_from(row);
        }
    }

    /// Ask the app for the file menu of this editor's file at `at`.
    fn open_file_menu(&self, cx: &mut EventCx, at: (f32, f32)) {
        if let Some(path) = self.buf.path.clone() {
            cx.request(Request::FileMenu {
                target: FileTarget { path, kind: TargetKind::File, origin: Origin::Editor },
                at,
            });
        }
    }

    /// Close the hover popup. Returns whether one was showing.
    fn dismiss_popup(&mut self) -> bool {
        self.lang.as_mut().is_some_and(Lang::dismiss)
    }

    /// Save to the backing file, reporting it to the app; without one, ask
    /// where to save.
    fn save(&mut self, cx: &mut EventCx) {
        let Some(path) = self.buf.path.clone() else {
            cx.request(Request::SaveAs);
            return;
        };
        if let Ok(true) = self.buf.save() {
            self.saved();
            cx.request(Request::Saved(path));
        }
    }

    /// The visual row at height `y`, from the last paint's layout (wrap- and
    /// scroll-aware): its logical line, the column it starts at, and how many
    /// characters it shows from there. `None` below the text.
    fn visual_row(&self, y: f32, cell_h: f32) -> Option<(usize, usize, usize)> {
        let l = self.layout;
        let e = &self.buf;
        let row_off = ((y - l.body.y) / cell_h).floor().max(0.0) as usize;
        // Walk visual rows from the scroll top to the one at `y`, as paint does.
        let mut vidx = 0usize;
        for ln in e.scroll..e.lines.len() {
            if l.wrap_cols == usize::MAX {
                if vidx == row_off {
                    // Unwrapped rows are shifted left by hscroll; a line shorter
                    // than that shows nothing and starts at its end.
                    let len = e.line_len(ln);
                    return Some((ln, e.hscroll.min(len), len.saturating_sub(e.hscroll)));
                }
                vidx += 1;
            } else {
                for (text, start) in wrap_line(&e.lines[ln], l.wrap_cols) {
                    if vidx == row_off {
                        return Some((ln, start, text.chars().count()));
                    }
                    vidx += 1;
                }
            }
        }
        None
    }

    /// The `(line, col)` caret position nearest a point; a point below the
    /// text lands at the end of the last line.
    fn pos_at(&self, x: f32, y: f32, cell_w: f32, cell_h: f32) -> Option<Pos> {
        let l = self.layout;
        if !rect_contains(l.body, x, y) {
            return None;
        }
        let col_off = ((x - l.text_x) / cell_w).round().max(0.0) as usize;
        match self.visual_row(y, cell_h) {
            Some((ln, start, len)) => Some((ln, (start + col_off).min(start + len))),
            None => {
                let last = self.buf.lines.len().saturating_sub(1);
                Some((last, self.buf.line_len(last)))
            }
        }
    }

    /// The character under a point, if it is over one.
    fn char_at(&self, x: f32, y: f32, cell_w: f32, cell_h: f32) -> Option<Pos> {
        let l = self.layout;
        if !rect_contains(l.body, x, y) || x < l.text_x {
            return None;
        }
        let col_off = ((x - l.text_x) / cell_w).floor() as usize;
        let (ln, start, len) = self.visual_row(y, cell_h)?;
        (col_off < len).then_some((ln, start + col_off))
    }

    /// The filename ribbon a single-tab editor shows in place of a tab: the name
    /// (italic when unsaved), a statistic (click to cycle), and the autosave and
    /// soft-wrap toggles.
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
        cx.tooltip(wb, "Soft wrap");
        let wb_color = if wrap { cx.chrome.accent } else { theme::GLYPH };
        cx.label("\u{21a9}", Font::MONO, wb.x + (wb.w - cw) * 0.5, top, wb, wb_color);
        // Autosave toggle (a floppy disk: green on, grey off) left of it; a
        // buffer with no file never autosaves, so there it is off and inert.
        let ab = Rect { x: wb.x - ch, ..wb };
        let has_file = self.buf.path.is_some();
        if has_file && cx.button(ab, BTN_AUTOSAVE) {
            cx.highlight(hover_box(ab, ch));
        }
        cx.tooltip(ab, self.autosave_tip(cx.cfg));
        let disk = if has_file && self.autosave_on(cx.cfg) { theme::ON } else { theme::DIM };
        paint_floppy(cx, ab, FLOPPY * ch, disk);
        // The statistic (click to cycle), right-aligned against the disk's
        // whole button (not just the glyph), so their hit areas never overlap.
        self.stats.poll();
        let spans = self.stats.spans(&self.buf);
        let stat = cx.shape_spans(&spans, Font::SANS, r.w);
        let gap = 6.0 * cx.ui.scale;
        let stat_left = ab.x - gap - stat.width;
        let sb = Rect { x: stat_left - gap, y: r.y, w: stat.width + gap * 2.0, h: r.h };
        if cx.button(sb, BTN_STAT) {
            cx.highlight(sb);
        }
        cx.tooltip(sb, self.stats.tip());
        cx.place(stat, stat_left, top, sb, theme::TEXT_DIM);
        // Filename, left-aligned, clipped before the statistic.
        let e = &self.buf;
        let name_clip = Rect { x: r.x, y: r.y, w: (sb.x - r.x).max(0.0), h: r.h };
        let font = Font::SANS.italic(e.modified);
        let name = cx.shape(&e.title(), font, name_clip.w);
        let name_w = name.width;
        cx.place(name, r.x + pad, top, name_clip, theme::TEXT);
        // With a file, the name opens its menu.
        self.ribbon = Some(r);
        self.name_hit = e.path.is_some().then(|| {
            let hit = Rect { x: r.x, y: r.y, w: (name_w + pad * 2.0).min(name_clip.w), h: r.h };
            if cx.button(hit, BTN_NAME) {
                cx.highlight(hit);
            }
            cx.tooltip(hit, "File actions");
            hit
        });
    }
}

/// The floppy-disk icon's side, in rows.
const FLOPPY: f32 = 0.6;

/// A floppy-disk icon `side` px square, centred in `r`: a body in `color` with
/// the shutter and label cut out in the ribbon colour.
fn paint_floppy(cx: &mut PaintCx, r: Rect, side: f32, color: [u8; 3]) {
    let x = r.x + (r.w - side) * 0.5;
    let y = r.y + (r.h - side) * 0.5;
    let bg = cx.chrome.sidebar;
    cx.fill(Rect { x, y, w: side, h: side }, color);
    cx.fill(Rect { x: x + side * 0.25, y, w: side * 0.45, h: side * 0.3 }, bg);
    cx.fill(Rect { x: x + side * 0.18, y: y + side * 0.55, w: side * 0.64, h: side * 0.3 }, bg);
}

/// A duration as a short "… ago" phrase.
fn ago(d: Duration) -> String {
    match d.as_secs() {
        s if s < 5 => "just now".to_string(),
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s => format!("{}h ago", s / 3600),
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
        if cx.modal {
            self.dismiss_popup();
        }
        let (cw, ch, scale) = (cx.ui.cell_w, cx.ui.cell_h, cx.ui.scale);
        let wrap = self.wrap_on(cx.cfg);
        // Without a tab strip, the filename gets a ribbon of its own.
        let mut body = rect;
        (self.ribbon, self.name_hit) = (None, None);
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

        self.sync_hl_states();
        let e = &self.buf;
        let (scroll, cursor, sel) = (e.scroll, e.cursor, e.selection());
        // Unwrapped rows hold their whole line, shaped once, drawn shifted left
        // by hscroll and clipped — moving the viewport rather than reshaping.
        let hscroll_px = e.hscroll as f32 * cw;
        let highlighter = e.path.as_deref().and_then(highlight::for_path);
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
            let len = text.chars().count();
            let (spans, styles) = match highlighter {
                Some(h) => {
                    let state = if h.carries_state() {
                        self.hl_states.state_in(h, &e.lines, e.edits, *ln)
                    } else {
                        0
                    };
                    let (hl, next) = self.hl_spans.line(h, &e.lines[*ln], state);
                    let block = h.block(&e.lines[*ln], state, next);
                    if block {
                        let r = Rect { x: text_x, y: *y, w: body.x + body.w - text_x, h: ch };
                        cx.fill_alpha(r, theme::CODE_BG, theme::CODE_BG_ALPHA);
                    }
                    syntax::styled_row_spans(&e.lines[*ln], hl, *start, len, EDITOR_FG, block)
                }
                None => (vec![(text.clone(), EDITOR_FG)], Vec::new()),
            };
            let styles = RowStyles { runs: &styles, cell_w: cw };
            let key = cx.row_key_styled(spans.iter().map(|(s, c)| (s.as_str(), *c)), styles);
            // Wide enough for the whole row, so layout is hscroll-independent.
            let shape_w = (len as f32 * cw).max(text_w);
            cx.budgeted_row_styled(key, || spans, styles, text_x - hscroll_px, *y, shape_w, text_clip, EDITOR_FG);
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
        if !cx.hovered(body) {
            self.dismiss_popup();
        }
        let Some(lang) = &self.lang else { return };
        let lines = &self.buf.lines;
        // Diagnostics: an underline under each visual row's part of the span.
        let thick = (1.5 * scale).max(1.0);
        for (start, end, color) in lang.underlines(lines) {
            for (i, (ln, text, row_start, y)) in vis.iter().enumerate() {
                if *ln < start.0 || *ln > end.0 {
                    continue;
                }
                // The span's part of this line, cut to this visual row. A line's
                // last row runs on past its text, so a span there (e.g. a missing
                // value) still shows.
                let from = if *ln == start.0 { start.1 } else { 0 };
                let to = if *ln == end.0 { end.1 } else { lines[*ln].chars().count() };
                let last_row = vis.get(i + 1).is_none_or(|next| next.0 != *ln);
                let row_end = if last_row { usize::MAX } else { row_start + text.chars().count() };
                let (a, b) = (from.max(*row_start), to.min(row_end));
                if b <= a {
                    continue;
                }
                let x0 = (text_x + (a - row_start) as f32 * cw - hscroll_px).max(text_x);
                let x1 = (text_x + (b - row_start) as f32 * cw - hscroll_px).min(body.x + body.w);
                if x1 > x0 {
                    cx.fill_top(Rect { x: x0, y: y + ch - thick, w: x1 - x0, h: thick }, color, 0.9);
                }
            }
        }
        if let Some(popup) = &lang.popup {
            let (line, col) = popup.at;
            let row = vis.iter().find(|(ln, text, start, _)| {
                *ln == line && *start <= col && col <= start + text.chars().count()
            });
            if let Some((_, _, start, y)) = row {
                let x = text_x + (col - start) as f32 * cw - hscroll_px;
                paint_popup(cx, popup, (x, *y), body);
            }
        }
    }

    /// Text editing per [`crate::plugin::text_edit`] (with undo/redo), plus
    /// Enter, Tab, and Cmd+S (save; Shift+S saves as).
    fn key(&mut self, cx: &mut EventCx, k: &KeyPress) -> bool {
        if self.dismiss_popup() {
            cx.redraw();
        }
        let m = k.mods;
        let used = match k.key {
            Key::Char(c) if m.super_ && c.eq_ignore_ascii_case(&'s') => {
                if m.shift {
                    cx.request(Request::SaveAs);
                } else {
                    self.save(cx);
                }
                true
            }
            Key::Enter if !m.super_ && !m.ctrl => {
                self.buf.apply(Edit::Insert("\n".into()), cx);
                true
            }
            Key::Tab if !m.super_ && !m.ctrl && !m.shift => {
                self.buf.apply(Edit::Insert(" ".repeat(TAB_WIDTH)), cx);
                true
            }
            _ => match edit_for(k) {
                Some(edit) => self.buf.apply(edit, cx).used(),
                None => false,
            },
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
            MouseEvent::Down { pos: (x, y), clicks } => {
                self.dismiss_popup();
                self.buf.end_undo_run();
                if let Some(p) = self.pos_at(x, y, cw, ch) {
                    self.buf.cursor = p;
                    self.buf.anchor = Some(p);
                    // A double-click selects the word under the pointer (the
                    // character it is over, not the nearest caret); a triple-click
                    // the line. The release keeps either.
                    if clicks == 2 {
                        let over = self.char_at(x, y, cw, ch).unwrap_or(p);
                        self.buf.select_word_at(over);
                    } else if clicks >= 3 {
                        self.buf.select_line_at(p.0);
                    }
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
                if !dragged && self.buf.anchor == Some(self.buf.cursor) {
                    self.buf.anchor = None;
                    cx.redraw();
                }
            }
            MouseEvent::Move { pos: (x, y) } => {
                let at = self.char_at(x, y, cw, ch);
                if self.lang.as_mut().is_some_and(|l| l.pointer(at, Instant::now())) {
                    cx.redraw();
                }
            }
            // The hover popup may extend over the next pane: leave it up.
            MouseEvent::Leave { .. } => {}
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
            self.dismiss_popup();
            cx.request_frame();
        }
    }

    fn context_menu(&mut self, cx: &mut EventCx, pos: (f32, f32)) {
        if self.ribbon.is_some_and(|r| rect_contains(r, pos.0, pos.1)) {
            self.open_file_menu(cx, pos);
        }
    }

    fn button(&mut self, cx: &mut EventCx, id: u32) {
        match id {
            BTN_NAME => {
                if let Some(hit) = self.name_hit {
                    self.open_file_menu(cx, (hit.x, hit.y + hit.h));
                }
                return;
            }
            BTN_WRAP => self.soft_wrap = Some(!self.wrap_on(cx.cfg)),
            BTN_STAT => self.stats.cycle(),
            BTN_AUTOSAVE => {
                let on = !self.autosave_on(cx.cfg);
                self.autosave = Some(on);
                // Switching on arms the idle timer for edits already pending.
                if on && self.buf.modified {
                    self.last_edit_at = Some(Instant::now());
                }
            }
            _ => return,
        }
        cx.redraw();
    }

    fn focus_changed(&mut self, cx: &mut EventCx, focused: bool) {
        if !focused && self.autosave_on_unfocus(cx.cfg) {
            if let Some(path) = self.save_if_modified() {
                cx.request(Request::Saved(path));
            }
        }
    }

    fn deadline(&self, cfg: &Config) -> Option<Instant> {
        let hover = self.lang.as_ref().and_then(Lang::deadline);
        self.autosave_deadline(cfg).into_iter().chain(hover).min()
    }

    fn tick(&mut self, cx: &mut EventCx, now: Instant) {
        if let Some(lang) = &mut self.lang {
            if lang.tick(&self.buf.lines, now) {
                cx.redraw();
            }
        }
        if self.autosave_deadline(cx.cfg).is_some_and(|d| now >= d) {
            self.last_edit_at = None;
            if let Some(path) = self.save_if_modified() {
                cx.request(Request::Saved(path));
            }
            cx.redraw();
        }
    }

    fn refresh(&mut self) -> bool {
        let stats = self.stats.poll();
        let lines = &self.buf.lines;
        stats | self.lang.as_mut().is_some_and(|l| l.refresh(lines))
    }

    fn autosave(&mut self, cfg: &Config) -> Option<PathBuf> {
        if !self.autosave_on_unfocus(cfg) {
            return None;
        }
        self.save_if_modified()
    }

    fn path(&self) -> Option<PathBuf> {
        self.buf.path.clone()
    }

    /// Creates missing parent directories, so a typed `sub/name` works. The
    /// old file's language service doesn't apply to the new one, so it stops.
    fn save_copy(&mut self, path: PathBuf) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        Ok(self.buf.save_copy(&path)?)
    }

    /// A file that moved is followed; its language service was started for
    /// the old path, so it stops.
    fn files_changed(&mut self, moved: &[(PathBuf, PathBuf)]) {
        let Some(to) = self.buf.path.as_deref().and_then(|p| crate::file_ops::follow(p, moved)) else {
            return;
        };
        self.buf.path = Some(to.clone());
        self.lang = None;
        self.stats.fetch_head(Some(to));
    }

    /// Unsaved edits stay: they are the user's, and saving them is a choice.
    fn changed_on_disk(&mut self, path: &Path) {
        let ours = self.buf.path.as_deref().is_some_and(|p| crate::languages::same_file(p, path));
        if ours && !self.buf.modified && self.buf.reload().unwrap_or(false) {
            self.note_edits();
        }
    }

    fn save_as(&mut self, path: PathBuf) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let moved = self.buf.path.as_ref() != Some(&path);
        self.buf.save_as(path)?;
        if moved {
            self.lang = None;
        }
        self.saved();
        Ok(())
    }

    /// Agent input goes in at the cursor as given (one undo step). It is
    /// pasted, not typed: typing a lone bracket auto-closes or wraps, which
    /// would make the result depend on how the input was chunked.
    fn write_input(&mut self, bytes: &[u8]) -> bool {
        let text = String::from_utf8_lossy(bytes).into_owned();
        self.buf.apply(Edit::Paste, &mut Some(text));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::testing::{cmd, press, tmpdir, Harness, UI};
    use crate::plugin::TextSrc;

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

    /// A language server that flags every "bad" and documents every word.
    struct Flags;

    impl ghostrealm_lsp::LanguageServer for Flags {
        fn name(&self) -> &str {
            "flags"
        }

        fn capabilities(&self) -> ghostrealm_lsp::types::ServerCapabilities {
            ghostrealm_lsp::types::ServerCapabilities {
                hover_provider: Some(ghostrealm_lsp::types::HoverProviderCapability::Simple(true)),
                ..Default::default()
            }
        }

        fn diagnostics(&mut self, doc: &ghostrealm_lsp::Document) -> Vec<ghostrealm_lsp::types::Diagnostic> {
            doc.text
                .match_indices("bad")
                .map(|(i, _)| ghostrealm_lsp::types::Diagnostic {
                    range: doc.range(i..i + 3),
                    message: "flagged".into(),
                    ..Default::default()
                })
                .collect()
        }

        fn hover(
            &mut self,
            doc: &ghostrealm_lsp::Document,
            pos: ghostrealm_lsp::types::Position,
        ) -> Option<ghostrealm_lsp::types::Hover> {
            use ghostrealm_lsp::types::{Hover, HoverContents, MarkupContent, MarkupKind};
            let off = doc.offset(pos)?;
            let start = doc.text[..off].rfind(' ').map_or(0, |i| i + 1);
            let end = doc.text[off..].find([' ', '\n']).map_or(doc.text.len(), |i| off + i);
            Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::PlainText,
                    value: format!("docs for {}", &doc.text[start..end]),
                }),
                range: Some(doc.range(start..end)),
            })
        }
    }

    fn attached(text: &str) -> EditorView {
        let mut v = view_with(text);
        v.attach(Client::in_process(Flags, None), "plaintext");
        v
    }

    /// Wait for the language service to post something.
    fn settle(v: &mut EditorView) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !v.refresh() {
            assert!(Instant::now() < deadline, "the language service never answered");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn popup_text(v: &EditorView) -> Option<Vec<String>> {
        let popup = v.lang.as_ref()?.popup.as_ref()?;
        Some(popup.text.iter().map(|(t, _)| t.clone()).collect())
    }

    #[test]
    fn diagnostics_are_underlined_and_follow_edits() {
        let mut h = Harness::new();
        let mut v = attached("ok bad\n");
        paint_in_strip(&mut h, &mut v);
        let plain = h.frame.top.len();
        settle(&mut v);
        paint_in_strip(&mut h, &mut v);
        assert_eq!(h.frame.top.len(), plain + 1, "one underline");

        v.buffer_mut().cursor = (1, 0);
        for c in "bad".chars() {
            h.key(&mut v, press(Key::Char(c)));
        }
        // Each keystroke is a change the service answers separately, so the
        // first answer can describe "b" or "ba", not "bad". Wait for the one
        // that flags the finished word.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            v.refresh();
            paint_in_strip(&mut h, &mut v);
            if h.frame.top.len() == plain + 2 {
                break;
            }
            assert!(Instant::now() < deadline, "the typed word is flagged too");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn resting_on_a_word_shows_its_diagnostic_then_its_docs() {
        let mut h = Harness::new();
        let mut v = attached("ok bad\n");
        settle(&mut v);
        paint_in_strip(&mut h, &mut v);

        let p = at(0, 4);
        h.cursor = p;
        h.mouse(&mut v, MouseEvent::Move { pos: p });
        assert_eq!(popup_text(&v), None, "nothing before the dwell");
        let due = v.deadline(&h.cfg).expect("a hover is due");
        h.tick(&mut v, due);
        assert_eq!(popup_text(&v), Some(vec!["flagged".to_string()]));
        settle(&mut v);
        assert_eq!(popup_text(&v), Some(vec!["flagged".to_string(), "docs for bad".to_string()]));
        paint_in_strip(&mut h, &mut v);
        assert!(!h.frame.overlay_text.is_empty(), "the popup paints above the panes");

        // Moving within the word keeps it; moving off closes it.
        h.mouse(&mut v, MouseEvent::Move { pos: at(0, 5) });
        assert!(popup_text(&v).is_some());
        h.mouse(&mut v, MouseEvent::Move { pos: at(0, 0) });
        assert_eq!(popup_text(&v), None);
    }

    #[test]
    fn a_modal_closes_the_popup() {
        let mut h = Harness::new();
        let mut v = attached("word\n");
        h.cursor = at(0, 1);
        paint_in_strip(&mut h, &mut v);
        settle(&mut v);
        h.mouse(&mut v, MouseEvent::Move { pos: at(0, 1) });
        let due = v.deadline(&h.cfg).unwrap();
        h.tick(&mut v, due);
        settle(&mut v);
        paint_in_strip(&mut h, &mut v);
        assert!(popup_text(&v).is_some(), "a plain repaint keeps it");
        assert!(!h.frame.overlay_text.is_empty());
        h.modal = true;
        paint_in_strip(&mut h, &mut v);
        assert_eq!(popup_text(&v), None);
        assert!(h.frame.overlay_text.is_empty(), "nothing paints over the modal");
        assert_eq!(v.deadline(&h.cfg), None, "and it doesn't come back under it");
    }

    #[test]
    fn typing_closes_the_popup() {
        let mut h = Harness::new();
        let mut v = attached("word\n");
        paint_in_strip(&mut h, &mut v);
        settle(&mut v);
        h.mouse(&mut v, MouseEvent::Move { pos: at(0, 1) });
        let due = v.deadline(&h.cfg).unwrap();
        h.tick(&mut v, due);
        settle(&mut v);
        assert!(popup_text(&v).is_some());
        h.key(&mut v, press(Key::Right));
        assert_eq!(popup_text(&v), None);
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
    fn the_statistic_ends_where_the_disk_button_begins_and_cycles_on_click() {
        let mut v = view_with("one two");
        v.buf.path = Some(tmpdir("editor-stat").join("f.txt"));
        let mut h = Harness::new();
        let buttons = h.paint(&mut v, RECT).view_buttons.clone();
        let at = |id| buttons.iter().find(|b| b.2 == id).unwrap().0;
        let (stat, disk) = (at(BTN_STAT), at(BTN_AUTOSAVE));
        assert!((stat.x + stat.w - disk.x).abs() < 1.0, "hit areas abut, never overlap");
        assert_eq!(v.stats.shown(), stats::Stat::Lines);
        h.button(&mut v, BTN_STAT);
        assert_eq!(v.stats.shown(), stats::Stat::Words);
    }

    #[test]
    fn the_ribbons_file_name_opens_the_file_menu_and_so_does_a_right_click_on_the_ribbon() {
        let path = tmpdir("editor-menu").join("f.txt");
        std::fs::write(&path, "x
").unwrap();
        let mut v = EditorView::new(EditorBuffer::open(path.clone()).unwrap());
        let mut h = Harness::new();
        let name = h.paint(&mut v, RECT).view_buttons.iter().find(|b| b.2 == BTN_NAME).expect("a name button").0;
        let target = FileTarget { path, kind: TargetKind::File, origin: Origin::Editor };
        let out = h.button(&mut v, BTN_NAME);
        assert_eq!(out.requests, [Request::FileMenu { target: target.clone(), at: (name.x, name.y + name.h) }]);
        // Right-click anywhere on the ribbon; not on the text.
        let at = (RECT.w - 2.0, 3.0);
        let out = h.context_menu(&mut v, at);
        assert_eq!(out.requests, [Request::FileMenu { target, at }]);
        assert!(h.context_menu(&mut v, (50.0, 100.0)).requests.is_empty(), "text has no menu");
        // Hovering the name highlights it (the button convention).
        let quads = |h: &Harness| h.frame.bg.len() + h.frame.top.len();
        h.cursor = (name.x + 1.0, name.y + 1.0);
        h.paint(&mut v, RECT);
        let hovered = quads(&h);
        h.cursor = (-1.0, -1.0);
        h.paint(&mut v, RECT);
        assert!(hovered > quads(&h), "hover adds a highlight quad");
    }

    #[test]
    fn a_scratch_buffer_or_a_tab_strip_has_no_file_menu() {
        let mut v = view_with("x");
        let mut h = Harness::new();
        let frame = h.paint(&mut v, RECT);
        assert!(frame.view_buttons.iter().all(|b| b.2 != BTN_NAME), "no file, no menu");
        assert!(h.context_menu(&mut v, (RECT.w - 2.0, 3.0)).requests.is_empty());
        let path = tmpdir("editor-menu-strip").join("f.txt");
        std::fs::write(&path, "x
").unwrap();
        let mut v = EditorView::new(EditorBuffer::open(path).unwrap());
        paint_in_strip(&mut h, &mut v);
        assert!(h.context_menu(&mut v, (RECT.w - 2.0, 3.0)).requests.is_empty(), "the tab strip has no ribbon");
    }

    #[test]
    fn a_moved_file_is_followed_and_a_copy_leaves_the_buffer_alone() {
        let dir = tmpdir("editor-moved");
        let old = dir.join("a.txt");
        std::fs::write(&old, "x
").unwrap();
        let mut v = EditorView::new(EditorBuffer::open(old.clone()).unwrap());
        v.files_changed(&[(dir.join("other"), dir.join("elsewhere"))]);
        assert_eq!(v.path(), Some(old.clone()), "unrelated moves don't matter");
        v.buffer_mut().insert_str("edit ");
        v.save_copy(dir.join("sub/copy.txt")).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("sub/copy.txt")).unwrap(), "edit x
");
        assert!(v.modified() && v.path() == Some(old.clone()));
        v.files_changed(&[(old, dir.join("b.txt"))]);
        assert_eq!(v.path(), Some(dir.join("b.txt")));
    }

    #[test]
    fn a_file_changed_on_disk_reloads_an_unmodified_buffer_and_spares_unsaved_edits() {
        let dir = tmpdir("editor-on-disk");
        let path = dir.join("config.toml");
        std::fs::write(&path, "a = 1\n").unwrap();
        let mut v = EditorView::new(EditorBuffer::open(path.clone()).unwrap());
        std::fs::write(&path, "a = 2\n").unwrap();
        v.changed_on_disk(&dir.join("other.toml"));
        assert_eq!(v.buffer().lines, ["a = 1"], "another file is not ours");
        v.changed_on_disk(&path);
        assert_eq!(v.buffer().lines, ["a = 2"]);
        assert!(!v.modified());
        // An edit and save now keep the other program's change.
        v.buffer_mut().cursor = (0, 5);
        v.buffer_mut().insert_str("0");
        assert_eq!(v.save_if_modified(), Some(path.clone()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a = 20\n");
        // Unsaved edits are not replaced.
        v.buffer_mut().insert_str("0");
        std::fs::write(&path, "a = 3\n").unwrap();
        v.changed_on_disk(&path);
        assert_eq!(v.buffer().lines, ["a = 200"]);
        assert!(v.modified());
    }

    #[test]
    fn a_scratch_buffer_shows_autosave_off_and_inert() {
        let mut v = view_with("x");
        let mut h = Harness::new();
        let frame = h.paint(&mut v, RECT);
        assert!(frame.view_buttons.iter().all(|b| b.2 != BTN_AUTOSAVE), "no toggle to click");
        assert_eq!(v.autosave_tip(&Config::default()), "Autosave off (no file)");
    }

    #[test]
    fn hovering_the_wrap_toggle_offers_a_tooltip() {
        let mut v = view_with("x");
        let mut h = Harness::new();
        let wrap = h.paint(&mut v, RECT).view_buttons.iter().find(|b| b.2 == BTN_WRAP).unwrap().0;
        assert!(h.frame.tooltip.is_none(), "not hovered");
        h.cursor = (wrap.x + 1.0, wrap.y + 1.0);
        let tip = h.paint(&mut v, RECT).tooltip.clone().map(|t| t.1);
        assert_eq!(tip.as_deref(), Some("Soft wrap"));
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
    fn double_click_selects_the_word_and_keeps_it_on_release() {
        let mut v = view_with("hello world");
        let mut h = Harness::new();
        paint_in_strip(&mut h, &mut v);
        h.mouse(&mut v, MouseEvent::Down { pos: at(0, 7), clicks: 2 });
        h.mouse(&mut v, MouseEvent::Up { pos: at(0, 7), dragged: false });
        assert_eq!(v.buffer().selected_text().as_deref(), Some("world"));
    }

    #[test]
    fn double_click_on_the_right_half_of_a_cell_selects_that_cells_word() {
        let mut v = view_with("hello world again");
        let mut h = Harness::new();
        paint_in_strip(&mut h, &mut v);
        let (x, y) = at(0, 10); // the `d` ending "world"
        let pos = (x + UI.cell_w * 0.75, y);
        h.mouse(&mut v, MouseEvent::Down { pos, clicks: 2 });
        h.mouse(&mut v, MouseEvent::Up { pos, dragged: false });
        assert_eq!(v.buffer().selected_text().as_deref(), Some("world"));
    }

    #[test]
    fn triple_click_selects_the_line() {
        let mut v = view_with("one two\nthree");
        let mut h = Harness::new();
        paint_in_strip(&mut h, &mut v);
        h.mouse(&mut v, MouseEvent::Down { pos: at(0, 5), clicks: 3 });
        h.mouse(&mut v, MouseEvent::Up { pos: at(0, 5), dragged: false });
        assert_eq!(v.buffer().selected_text().as_deref(), Some("one two"));
    }

    #[test]
    fn a_click_past_a_short_scrolled_line_lands_at_its_end() {
        let mut v = view_with(&format!("ab\n{}", "x".repeat(100)));
        v.soft_wrap = Some(false);
        v.buf.hscroll = 50;
        let mut h = Harness::new();
        paint_in_strip(&mut h, &mut v);
        h.mouse(&mut v, MouseEvent::Down { pos: at(0, 3), clicks: 1 });
        h.mouse(&mut v, MouseEvent::Up { pos: at(0, 3), dragged: false });
        assert_eq!(v.buffer().cursor, (0, 2));
        h.key(&mut v, press(Key::Backspace));
        assert_eq!(v.buffer().lines[0], "a");
    }

    #[test]
    fn typing_after_a_mouse_selection_is_its_own_undo_step() {
        let mut v = view_with("");
        let mut h = Harness::new();
        paint_in_strip(&mut h, &mut v);
        for c in "abc".chars() {
            h.key(&mut v, press(Key::Char(c)));
        }
        h.mouse(&mut v, MouseEvent::Down { pos: at(0, 0), clicks: 1 });
        h.mouse(&mut v, MouseEvent::Drag { pos: at(0, 3) });
        h.mouse(&mut v, MouseEvent::Up { pos: at(0, 3), dragged: true });
        h.key(&mut v, press(Key::Char('x')));
        assert_eq!(v.buffer().lines, vec!["x"]);
        h.key(&mut v, cmd(Key::Char('z')));
        assert_eq!(v.buffer().lines, vec!["abc"]);
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
        let mut v = EditorView::new(EditorBuffer::open(path.clone()).unwrap());
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
        let mut v = EditorView::new(EditorBuffer::open(path).unwrap());
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
    fn a_failed_save_as_keeps_the_old_file() {
        let dir = tmpdir("editor-save-as-fail");
        let old = dir.join("old.txt");
        std::fs::write(&old, "x").unwrap();
        let mut v = EditorView::new(EditorBuffer::open(old.clone()).unwrap());
        // A directory can't be written as a file.
        assert!(v.save_as(dir.clone()).is_err());
        assert_eq!(v.path(), Some(old));
    }

    #[test]
    fn option_moves_by_word_and_cmd_shift_selects_to_line_start() {
        let mut v = view_with("hello big world");
        let h = Harness::new();
        h.key(&mut v, cmd(Key::Right));
        let mut opt_left = press(Key::Left);
        opt_left.mods.alt = true;
        h.key(&mut v, opt_left);
        assert_eq!(v.buffer().cursor, (0, 10));
        let mut cmd_shift_left = cmd(Key::Left);
        cmd_shift_left.mods.shift = true;
        h.key(&mut v, cmd_shift_left);
        assert_eq!(v.buffer().selected_text().as_deref(), Some("hello big "));
        let mut opt_bs = press(Key::Backspace);
        opt_bs.mods.alt = true;
        h.key(&mut v, opt_bs);
        assert_eq!(v.buffer().lines, vec!["world"]);
    }

    #[test]
    fn cmd_z_undoes_and_cmd_shift_z_redoes() {
        let mut v = view_with("");
        let h = Harness::new();
        for c in "hi".chars() {
            h.key(&mut v, press(Key::Char(c)));
        }
        h.key(&mut v, press(Key::Enter));
        h.key(&mut v, cmd(Key::Char('z')));
        assert_eq!(v.buffer().lines, vec!["hi"]);
        let mut redo = cmd(Key::Char('Z'));
        redo.mods.shift = true;
        let (used, out) = h.key(&mut v, redo);
        assert!(used && out.redraw);
        assert_eq!(v.buffer().lines, vec!["hi", ""]);
    }

    #[test]
    fn highlighted_files_paint_every_row_through_edits_and_scrolling() {
        let dir = tmpdir("editor-highlight");
        let path = dir.join("doc.md");
        let text: String = std::iter::once("```".to_string())
            .chain((1..60).map(|i| format!("# line {i}")))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, text).unwrap();
        let mut v = EditorView::new(EditorBuffer::open(path).unwrap());
        let mut h = Harness::new();
        let rows = (RECT.h / UI.cell_h) as usize;
        for scroll in [0, 20, 52] {
            v.buf.scroll = scroll;
            h.key(&mut v, press(Key::Char('x')));
            let drawn = h.paint(&mut v, RECT).text.len();
            assert!(drawn >= rows.min(60 - scroll) - 1, "scroll {scroll}: {drawn} rows");
        }
    }

    fn markdown_view(tag: &str, text: &str) -> EditorView {
        let path = tmpdir(tag).join("doc.md");
        std::fs::write(&path, text).unwrap();
        EditorView::new(EditorBuffer::open(path).unwrap())
    }

    /// The bg quads of a painted frame as pixel rects `(x, y, w, h)`.
    fn bg_rects(h: &Harness) -> Vec<(f32, f32, f32, f32)> {
        let (sw, sh) = h.frame.screen;
        h.frame
            .bg
            .iter()
            .map(|q| {
                (
                    ((q.pos[0] + 1.0) / 2.0 * sw).round(),
                    ((1.0 - q.pos[1]) / 2.0 * sh).round(),
                    (q.size[0] / 2.0 * sw).round(),
                    (-q.size[1] / 2.0 * sh).round(),
                )
            })
            .collect()
    }

    /// The source of the text drawn in the editor's text column on visual row `row`.
    fn text_item(h: &Harness, row: usize) -> TextSrc {
        let (x, y) = at(row, 0);
        h.frame.text.iter().find(|t| t.left == x && (t.top - y).abs() < 2.0).expect("a text row").src
    }

    #[test]
    fn plain_markdown_rows_draw_exactly_as_unhighlighted_ones() {
        let mut h = Harness::new();
        let text = "hello world\n- item one\n> quote";
        let mut md = markdown_view("editor-md-plain", text);
        paint_in_strip(&mut h, &mut md);
        let md_quads = bg_rects(&h).len();
        let row0 = text_item(&h, 0);
        let mut txt = view_with(text);
        paint_in_strip(&mut h, &mut txt);
        assert_eq!(bg_rects(&h).len(), md_quads, "no decoration quads for plain rows");
        // The first row has one span in the plain colour: the key a plain row always had.
        let key = h.text.row_cache.row_key(std::iter::once(("hello world", EDITOR_FG)));
        assert_eq!(row0, TextSrc::Row(key));
        paint_in_strip(&mut h, &mut txt);
        assert_eq!(text_item(&h, 0), TextSrc::Row(key), "same as with no highlighter");
    }

    #[test]
    fn markdown_styles_draw_underline_and_code_box_quads_and_keep_the_markers() {
        let mut h = Harness::new();
        let mut plain = markdown_view("editor-md-plain2", "see the docs now");
        paint_in_strip(&mut h, &mut plain);
        let base = bg_rects(&h);

        let mut v = markdown_view("editor-md-link", "see [the docs](u) now");
        paint_in_strip(&mut h, &mut v);
        let got = bg_rects(&h);
        // "the docs" starts at column 5 and is 8 cells wide; the underline is a thin quad at the row bottom.
        let (text_x, cw) = (4.0 * UI.cell_w, UI.cell_w);
        let under: Vec<_> = got.iter().filter(|r| !base.contains(r) && r.3 < 3.0).collect();
        assert_eq!(under.len(), 1, "{got:?}");
        assert_eq!((under[0].0, under[0].2), (text_x + 5.0 * cw, 8.0 * cw));
        assert!(under[0].1 >= UI.cell_h - 3.0 && under[0].1 < UI.cell_h, "under the glyphs: {under:?}");
        // The markup is still in the document and drawn: the whole line is shaped text.
        assert_eq!(v.buffer().lines[0], "see [the docs](u) now");

        let mut v = markdown_view("editor-md-code", "a `code` b");
        paint_in_strip(&mut h, &mut v);
        let boxes: Vec<_> = bg_rects(&h).into_iter().filter(|r| !base.contains(r) && r.3 > 3.0).collect();
        assert_eq!(boxes.len(), 1, "{boxes:?}");
        assert_eq!((boxes[0].0, boxes[0].2), (text_x + 3.0 * cw, 4.0 * cw), "the code text, not the backticks");
    }

    #[test]
    fn fenced_blocks_get_a_full_width_background_and_their_language_colours() {
        let mut h = Harness::new();
        let mut v = markdown_view("editor-md-fence", "text\n```toml\nname = 1\n```\nafter");
        paint_in_strip(&mut h, &mut v);
        let text_x = 4.0 * UI.cell_w;
        let block: Vec<_> = bg_rects(&h)
            .into_iter()
            .filter(|r| r.0 == text_x && r.2 == RECT.w - text_x && r.3 == UI.cell_h)
            .collect();
        let ys: Vec<f32> = block.iter().map(|r| r.1).collect();
        // The cursor line also spans the width; the fence rows are lines 1..=3.
        for row in 1..=3 {
            assert!(ys.contains(&(row as f32 * UI.cell_h)), "row {row} tinted: {ys:?}");
        }
        assert!(!ys.contains(&(4.0 * UI.cell_h)), "prose after the block is not");
        // The body is coloured as TOML (a key, an operator, a number), not as one run of code.
        let toml_row = [("name ", syntax::color(highlight::Token::Key, EDITOR_FG)), ("= ", EDITOR_FG),
            ("1", syntax::color(highlight::Token::Number, EDITOR_FG))];
        let key = h.text.row_cache.row_key(toml_row.into_iter());
        assert_eq!(text_item(&h, 2), TextSrc::Row(key));
    }

    #[test]
    fn typed_brackets_auto_close_wrap_and_step_over_through_the_key_path() {
        let h = Harness::new();
        let mut v = view_with("");
        for c in "f(x)".chars() {
            h.key(&mut v, press(Key::Char(c)));
        }
        assert_eq!(v.buffer().lines, vec!["f(x)"]);
        h.key(&mut v, cmd(Key::Char('a')));
        h.key(&mut v, press(Key::Char('[')));
        assert_eq!(v.buffer().lines, vec!["[f(x)]"], "a selection is wrapped");
        h.key(&mut v, cmd(Key::Char('z')));
        assert_eq!(v.buffer().lines, vec!["f(x)"]);
    }

    #[test]
    fn a_comment_inside_a_markdown_code_block_uses_the_blocks_language() {
        let dir = tmpdir("editor-comment-fence");
        let path = dir.join("a.md");
        std::fs::write(&path, "text\n```toml\na = 1\n```\n```zzz\nx\n```\n").unwrap();
        let mut v = EditorView::new(EditorBuffer::open(path).unwrap());
        v.buf.cursor = (2, 0);
        v.toggle_comment().unwrap();
        assert_eq!(v.buffer().lines[2], "# a = 1");
        v.buf.cursor = (0, 0);
        v.toggle_comment().unwrap();
        assert_eq!(v.buffer().lines[0], "<!-- text -->");
        v.buf.cursor = (5, 0);
        assert!(v.toggle_comment().is_err(), "a block in an unknown language has no comment syntax");
        assert_eq!(v.buffer().lines[5], "x");
    }

    #[test]
    fn agent_input_is_literal_text_whatever_its_chunks() {
        let mut v = view_with("");
        for chunk in ["f", "(", "x)"] {
            v.write_input(chunk.as_bytes());
        }
        assert_eq!(v.buffer().lines, vec!["f(x)"], "no closer is added");
        v.buf.select_all();
        v.write_input(b"[");
        assert_eq!(v.buffer().lines, vec!["["], "a selection is replaced, not wrapped");
    }

    #[test]
    fn highlight_state_follows_edits_reported_by_the_buffer() {
        let dir = tmpdir("editor-highlight-edits");
        let path = dir.join("doc.md");
        std::fs::write(&path, "a\n```\nb\nc\n```\nd\n").unwrap();
        let mut v = EditorView::new(EditorBuffer::open(path).unwrap());
        let mut h = Harness::new();
        let md = highlight::for_extension("md").unwrap();
        // The state each line enters with, by the cache and by a full pass.
        let check = |v: &mut EditorView| {
            let mut state = 0;
            for (i, l) in v.buf.lines.clone().iter().enumerate() {
                let got = v.hl_states.state_in(md, &v.buf.lines, v.buf.edits, i);
                assert_eq!(got, state, "line {i} of {:?}", v.buf.lines);
                state = md.line(l, state).1;
            }
        };
        paint_in_strip(&mut h, &mut v);
        check(&mut v);
        // Delete the opening fence's first backtick: the block is no longer a fence.
        v.buf.cursor = (1, 0);
        h.key(&mut v, press(Key::Delete));
        paint_in_strip(&mut h, &mut v);
        check(&mut v);
        // Type it back, then undo.
        h.key(&mut v, press(Key::Char('`')));
        paint_in_strip(&mut h, &mut v);
        check(&mut v);
        h.key(&mut v, cmd(Key::Char('z')));
        paint_in_strip(&mut h, &mut v);
        check(&mut v);
        // A new line above shifts the fence down.
        v.buf.cursor = (0, 1);
        h.key(&mut v, press(Key::Enter));
        paint_in_strip(&mut h, &mut v);
        check(&mut v);
    }

    #[test]
    fn save_as_elsewhere_stops_the_language_service() {
        let path = tmpdir("editor-save-as-lang").join("copy.toml");
        let mut v = attached("bad\n");
        v.save_as(path).unwrap();
        assert!(v.lang().is_none());
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
        let mut v = EditorView::new(EditorBuffer::open(path.clone()).unwrap());
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
        let mut v = EditorView::new(EditorBuffer::open(path.clone()).unwrap());
        let h = Harness::new();
        h.key(&mut v, press(Key::Char('z')));
        let out = h.focus(&mut v, false);
        assert_eq!(out.requests, vec![Request::Saved(path)]);
    }

    #[test]
    fn the_autosave_toggle_stops_idle_and_unfocus_saves() {
        let path = tmpdir("editor-autosave-off").join("f.txt");
        let mut v = EditorView::new(EditorBuffer::open(path.clone()).unwrap());
        let h = Harness::new();
        assert!(v.autosave_on(&h.cfg), "the config's default is on");
        h.button(&mut v, BTN_AUTOSAVE);
        h.key(&mut v, press(Key::Char('a')));
        assert_eq!(v.deadline(&h.cfg), None, "no idle timer");
        assert!(h.focus(&mut v, false).requests.is_empty(), "no save on unfocus");
        assert_eq!(v.autosave(&h.cfg), None, "nor on quit");
        assert!(v.modified());
    }

    #[test]
    fn switching_autosave_on_works_even_when_the_config_has_it_off() {
        let path = tmpdir("editor-autosave-on").join("f.txt");
        let mut v = EditorView::new(EditorBuffer::open(path.clone()).unwrap());
        let mut h = Harness::new();
        h.cfg.editor.autosave_after = 0;
        h.cfg.editor.autosave_on_unfocus = false;
        h.key(&mut v, press(Key::Char('a')));
        assert!(!v.autosave_on(&h.cfg) && v.deadline(&h.cfg).is_none());
        h.button(&mut v, BTN_AUTOSAVE);
        let due = v.deadline(&h.cfg).expect("pending edits arm the timer");
        assert_eq!(h.tick(&mut v, due).requests, vec![Request::Saved(path)]);
        assert!(v.autosave_tip(&h.cfg).starts_with("Autosave on\nLast save: just now"));
    }

    #[test]
    fn ago_reads_in_the_largest_unit() {
        assert_eq!(ago(Duration::from_secs(2)), "just now");
        assert_eq!(ago(Duration::from_secs(42)), "42s ago");
        assert_eq!(ago(Duration::from_secs(185)), "3m ago");
        assert_eq!(ago(Duration::from_secs(7300)), "2h ago");
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
