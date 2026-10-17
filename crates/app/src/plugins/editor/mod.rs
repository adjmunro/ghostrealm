//! The text editor plugin: a minimal editable buffer with line numbers,
//! soft-wrap, selection, and syntax highlighting. It claims any file path, so it
//! is the fallback when no more specific plugin opens a file. A file with a
//! language service shows its diagnostics and hover docs.

pub mod buffer;
mod stats;
pub mod lang;
mod rows;
pub mod wrap;

use std::any::Any;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ghostrealm_core::config::{Kind, OptionSpec, SectionSpec};
use ghostrealm_core::highlight::{self, BlockRow, Highlighter, SpanCache, StateCache};
use ghostrealm_core::{
    ArgKind, ArgSpec, CmdError, CmdOutcome, CommandMeta, Config, LineNumbers, Rect, Registry,
};
use ghostrealm_lsp::Client;
use ghostrealm_terminal::{Key, KeyPress};

use crate::app_state::AppState;
use crate::file_menu::{FileTarget, Kind as TargetKind, Origin};
use crate::syntax;
use crate::plugin::icons::Icon;
use crate::plugin::text_edit::{edit_for, Edit};
use crate::plugin::{
    hover_box, rect_contains, theme, EventCx, Font, MenuAlign, MenuItem, MouseEvent, OpenCx, PaintCx, Plugin,
    Request, RowStyles, View, Waker,
};
use crate::plugins::files::header::{text_kind, DocHeader, HeaderText};

pub use buffer::EditorBuffer;
use stats::{Stat, Stats};
use lang::{paint_popup, Lang, Pos};
use rows::{RowKind, RowLayout, RowTable, TextRow};

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

/// The header's buttons and `⋯` menu items (see [`View::button`]). The
/// statistic items take a range from `BTN_SHOW_STAT` up, so every other id
/// stays below it.
const BTN_WRAP: u32 = 0;
const BTN_AUTOSAVE: u32 = 1;
const BTN_STAT: u32 = 2;
const BTN_NAME: u32 = 3;
const BTN_MORE: u32 = 4;
/// The `⋯` menu's "File actions…": the file menu, where the `⋯` menu was.
const BTN_FILE_MENU: u32 = 5;
/// Menu items showing statistic `Stat::ALL[i]` are `BTN_SHOW_STAT + i`.
const BTN_SHOW_STAT: u32 = 16;
/// The copy button of the code block opening on line `ln` is
/// `BTN_COPY_BLOCK + ln`: tied to the block, not to where it is on screen.
const BTN_COPY_BLOCK: u32 = 256;
/// How long a code block's copy button shows a check mark after a copy.
const COPIED_FOR: Duration = Duration::from_millis(1500);
const _: () = assert!(
    BTN_SHOW_STAT > BTN_WRAP
        && BTN_SHOW_STAT > BTN_AUTOSAVE
        && BTN_SHOW_STAT > BTN_STAT
        && BTN_SHOW_STAT > BTN_NAME
        && BTN_SHOW_STAT > BTN_MORE
        && BTN_SHOW_STAT > BTN_FILE_MENU,
    "the statistic items' ids must not collide with another button's"
);
const _: () = assert!(
    BTN_COPY_BLOCK >= BTN_SHOW_STAT + Stat::ALL.len() as u32,
    "the copy buttons' ids must not collide with a statistic item's"
);
/// The idle-autosave delay (seconds) when autosave is switched on in the header
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

    /// An editor is a Files surface's document, not a surface of its own.
    fn pickable(&self) -> bool {
        false
    }

    /// An empty buffer, as a document (see `AppState::open_scratch`).
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
            CommandMeta::new("editor.new", "New Editor", "Open an empty text document in a Files surface"),
            Box::new(move |s: &mut AppState, _| {
                s.open_scratch().map_err(failed)?;
                Ok(CmdOutcome::ok())
            }),
        );
        r.register(
            CommandMeta::new(
                "editor.open",
                "Open File in Editor",
                "Open a file in a Files surface: the focused one, else the workspace's last focused one, else a new one",
            )
            .arg(ArgSpec::optional("path", ArgKind::Str, "the file to open (chosen in a picker unless given)")),
            Box::new(move |s: &mut AppState, a| {
                if a.get("path").is_none() {
                    s.open_picker(crate::app_state::Pick::Open);
                    return Ok(CmdOutcome::ok());
                }
                let path = PathBuf::from(a.get_str("path")?);
                s.open_file(&path).map_err(failed)?;
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
                let doc = s.focused_surface().and_then(|sid| s.active_doc(sid));
                let editor = doc
                    .and_then(|d| s.doc_mut(d))
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
                    .filter(|sid| s.active_doc(*sid).and_then(|d| s.doc(d)).is_some_and(|v| v.plugin() == ID))
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
                    "Wrap long lines at the pane edge (toggle per document with the header's button). \
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
    /// The text area including the gutter (below any header).
    body: Rect,
    /// Left edge of the text (right of the gutter).
    text_x: f32,
    /// How lines break into rows.
    rows: RowLayout,
}

pub struct EditorView {
    buf: EditorBuffer,
    /// This view's soft-wrap override; `None` follows `[editor] soft_wrap`.
    soft_wrap: Option<bool>,
    /// This view's autosave override (the header toggle); `None` follows
    /// `[editor] autosave_after` / `autosave_on_unfocus`.
    autosave: Option<bool>,
    /// When the buffer was last written (any save).
    last_saved: Option<Instant>,
    /// The header's statistic (click to cycle).
    stats: Stats,
    /// Sub-line wheel remainder (px) carried between events so slow scrolls
    /// accumulate instead of rounding away.
    scroll_accum: f32,
    /// The buffer's edit count when last looked at, and when an edit was last
    /// seen (drives the idle autosave).
    seen_edits: u64,
    last_edit_at: Option<Instant>,
    layout: Layout,
    /// The rows the last paint laid out (relaid when an event finds them
    /// stale).
    rows: RowTable,
    /// Draw the document header above the text: the view is a Files
    /// surface's document.
    header_on: bool,
    header: DocHeader,
    /// The header's file name and the header itself, as last painted (none
    /// without a header, the name none without a file): they open the file
    /// menu.
    name_hit: Option<Rect>,
    header_rect: Option<Rect>,
    /// The header's `⋯` button, as last painted: its menu opens below it.
    more_hit: Option<Rect>,
    /// The file's language service, if it has one.
    lang: Option<Lang>,
    /// Where each line's syntax-highlight state starts.
    hl_states: StateCache,
    /// Highlight of the lines on screen, kept between frames.
    hl_spans: SpanCache,
    /// The code blocks on screen, as last painted.
    code_blocks: Vec<ShownBlock>,
    /// The first line on screen of the code block the last paint found under
    /// the pointer (and showed its copy button): a pointer move that changes
    /// it repaints.
    hover_block: Option<usize>,
    /// For the hovered block, keyed on its first line on screen and the
    /// buffer's edit count: its opening line and whether its body has text,
    /// so a hovered block deep in a long one isn't walked back every frame.
    hover_open: Option<((usize, u64), Option<usize>, bool)>,
    /// The opening line of the code block last copied, and until when its
    /// button shows a check mark.
    copied: Option<(usize, Instant)>,
}

/// A fenced code block on screen.
#[derive(Clone, Copy, Debug)]
struct ShownBlock {
    /// Its box, cut to the text body.
    rect: Rect,
    /// Its first line on screen.
    first: usize,
}

impl EditorView {
    pub fn new(buf: EditorBuffer) -> Self {
        Self::with_waker(buf, None)
    }

    /// Like [`Self::new`], waking the UI with `waker` when background work
    /// (the header's statistic) finishes.
    pub fn with_waker(buf: EditorBuffer, waker: Option<Waker>) -> Self {
        let mut stats = Stats::new(waker);
        stats.fetch_head(buf.path.clone());
        EditorView {
            hl_states: StateCache::default(),
            hl_spans: SpanCache::default(),
            code_blocks: Vec::new(),
            hover_block: None,
            hover_open: None,
            copied: None,
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
                rows: RowLayout { text_w: 0.0, cell: (0.0, 0.0), wrap: false },
            },
            rows: RowTable::default(),
            lang: None,
            header_on: false,
            header: DocHeader::default(),
            name_hit: None,
            header_rect: None,
            more_hit: None,
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

    /// Draw the document header above the text (its host, a Files surface,
    /// sets it).
    pub fn set_header(&mut self, on: bool) {
        self.header_on = on;
        self.header.refresh(self.buf.path.as_deref());
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

    /// Write a modified, file-backed buffer. Returns the file written. A file
    /// that changed on disk since the buffer loaded or wrote it is left alone,
    /// and the buffer stays modified: only an explicit save may ask to
    /// overwrite it.
    fn save_if_modified(&mut self) -> Option<PathBuf> {
        if !self.buf.modified || self.buf.path.is_none() {
            return None;
        }
        if self.buf.conflicted() {
            self.buf.changed_on_disk = true;
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
        self.header.refresh(self.buf.path.as_deref());
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

    /// Comment or uncomment the selected lines per the file's language, or
    /// per an embedded language when every line takes its syntax
    /// ([`highlight::Highlighter::comment_on`]: all inside one Markdown code
    /// block). A file without a highlighter uses its [`highlight::Language`].
    /// Returns `Err` when the language has no comment syntax.
    pub fn toggle_comment(&mut self) -> Result<(), &'static str> {
        const NONE: &str = "this language has no comment syntax";
        let path = self.buf.path.as_deref().ok_or(NONE)?;
        let mut lead = Vec::new();
        let syntax = match highlight::for_path(path) {
            Some(h) if h.carries_state() => {
                let (first, last) = self.buf.line_rows();
                self.sync_hl_states();
                let mut on = |row: usize| {
                    let state = self.hl_states.state_in(h, &self.buf.lines, self.buf.edits, row);
                    let line = &self.buf.lines[row];
                    (h.comment_on(line, state), h.comment_lead(line, state))
                };
                let embedded = on(first).0;
                // A closing fence or host text between two blocks is the host's.
                for r in first..=last {
                    let (syntax, at) = on(r);
                    if syntax != embedded {
                        lead.clear();
                        break;
                    }
                    lead.push(at);
                }
                if lead.is_empty() {
                    h.comment()
                } else {
                    embedded
                }
            }
            Some(h) => h.comment(),
            None => highlight::language_for_path(path).map(|l| l.comment).unwrap_or_default(),
        };
        if syntax == highlight::CommentSyntax::default() {
            return Err(NONE);
        }
        if self.buf.toggle_comment(syntax, &lead) {
            self.note_edits();
        }
        Ok(())
    }

    /// The body of the fenced block opening on line `open`, without its
    /// fences, as a snippet pasted elsewhere wants it: each row without the
    /// opening fence's quote markers (`>`, each with one optional space) and
    /// the indent of its container (the fence's own indent, or a list item's
    /// content column), and an unclosed block without its trailing blank
    /// rows. `None` when `open` doesn't open a block or the body is empty.
    fn block_body(&mut self, open: usize) -> Option<String> {
        let h = self.buf.path.as_deref().and_then(highlight::for_path)?;
        self.sync_hl_states();
        block_text(&mut self.hl_states, &mut self.hl_spans, &self.buf, h, open)
    }

    /// Tell the highlight state cache which lines the edits since it last
    /// heard have changed.
    fn sync_hl_states(&mut self) {
        if let Some(row) = self.buf.dirty_from.take() {
            self.hl_states.dirty_from(row);
        }
    }

    /// Ask the app for the file menu of this editor's file at `at`.
    fn open_file_menu(&self, cx: &mut EventCx, at: (f32, f32), align: MenuAlign) {
        if let Some(path) = self.buf.path.clone() {
            cx.request(Request::FileMenu {
                target: FileTarget { path, kind: TargetKind::File, origin: Origin::Document },
                at,
                align,
            });
        }
    }

    /// Where the `⋯` menu opens: right-aligned below the button, so it stays
    /// over the editor.
    fn more_anchor(&self) -> Option<(f32, f32)> {
        self.more_hit.map(|mb| (mb.x + mb.w, mb.y + mb.h))
    }

    /// The header's `⋯` menu: every header option, checked as it stands.
    fn header_menu(&self, cfg: &Config) -> Vec<MenuItem> {
        let has_file = self.buf.path.is_some();
        let mut items = vec![
            MenuItem::new(BTN_WRAP, "Soft wrap").check(self.wrap_on(cfg)),
            MenuItem::new(BTN_AUTOSAVE, "Autosave")
                .check(has_file && self.autosave_on(cfg))
                .enable(has_file),
            MenuItem::RULE,
        ];
        for (i, stat) in Stat::ALL.into_iter().enumerate() {
            if self.stats.offers(stat) {
                items.push(MenuItem::new(BTN_SHOW_STAT + i as u32, stat.name()).check(self.stats.shown() == stat));
            }
        }
        if has_file {
            items.push(MenuItem::RULE);
            items.push(MenuItem::new(BTN_FILE_MENU, "File actions…"));
        }
        items
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
        if self.buf.conflicted() {
            cx.request(Request::SaveConflict);
            return;
        }
        if let Ok(true) = self.buf.save() {
            self.saved();
            cx.request(Request::Saved(path));
        }
    }

    /// Lay out the rows from the scroll line in the last paint's layout.
    fn layout_rows(&mut self) {
        let l = self.layout;
        let h = self.highlighter();
        let (states, buf) = (&mut self.hl_states, &self.buf);
        let span = (l.body.y, l.body.y + l.body.h);
        let name = h.map(|h| h.name());
        self.rows = RowTable::build(&buf.lines, buf.scroll, buf.edits, l.rows, name, span, |ln| {
            line_scale(states, buf, h, ln)
        });
    }

    /// The highlighter for the buffer's file, if any.
    fn highlighter(&self) -> Option<&'static dyn Highlighter> {
        self.buf.path.as_deref().and_then(highlight::for_path)
    }

    /// The row at height `y` in the last paint's layout (wrap-, scroll- and
    /// size-aware), relaid if a scroll or an edit since has made it stale:
    /// its logical line, the column it starts at, how many characters it
    /// shows from there, and its scale. A row above the text is the first;
    /// `None` below the text.
    fn visual_row(&mut self, y: f32) -> Option<(RowKind, f32)> {
        let name = self.highlighter().map(|h| h.name());
        if !self.rows.fresh(self.buf.scroll, self.buf.edits, self.layout.rows, name) {
            self.sync_hl_states();
            self.layout_rows();
        }
        let row = *self.rows.at(y)?;
        let kind = match row.kind {
            // Unwrapped rows are shifted left by hscroll; a line shorter than
            // that shows nothing and starts at its end.
            RowKind::Text { ln, len, .. } if !self.layout.rows.wrap => {
                let hs = self.buf.hscroll;
                RowKind::Text { ln, start: hs.min(len), len: len.saturating_sub(hs) }
            }
            kind => kind,
        };
        Some((kind, row.scale))
    }

    /// The `(line, col)` caret position nearest a point; a point below the
    /// text lands at the end of the last line, one on a widget at the end of
    /// the line above it.
    fn pos_at(&mut self, x: f32, y: f32) -> Option<Pos> {
        let l = self.layout;
        if !rect_contains(l.body, x, y) {
            return None;
        }
        match self.visual_row(y) {
            Some((RowKind::Text { ln, start, len }, scale)) => {
                let col_off = ((x - l.text_x) / (l.rows.cell.0 * scale)).round().max(0.0) as usize;
                Some((ln, (start + col_off).min(start + len)))
            }
            Some((RowKind::Widget { after_ln, .. }, _)) => Some((after_ln, self.buf.line_len(after_ln))),
            None => {
                let last = self.buf.lines.len().saturating_sub(1);
                Some((last, self.buf.line_len(last)))
            }
        }
    }

    /// The character under a point, if it is over one.
    fn char_at(&mut self, x: f32, y: f32) -> Option<Pos> {
        let l = self.layout;
        if !rect_contains(l.body, x, y) || x < l.text_x {
            return None;
        }
        let (RowKind::Text { ln, start, len }, scale) = self.visual_row(y)? else { return None };
        let col_off = ((x - l.text_x) / (l.rows.cell.0 * scale)).floor() as usize;
        (col_off < len).then_some((ln, start + col_off))
    }

    /// The document header: the file's details, with the controls at the
    /// right of the name's line: a statistic (click to cycle), the autosave
    /// and soft-wrap toggles, and `⋯` (a menu of every option).
    fn paint_header(&mut self, cx: &mut PaintCx, r: Rect, wrap: bool) {
        let (cw, ch) = (cx.ui.cell_w, cx.ui.cell_h);
        let row = DocHeader::control_row(cx.ui, r);
        let side = row.h;
        let top = row.y + (row.h - ch) * 0.5;
        // Laid out right to left first: the header's text stops at them.
        let mb = Rect { x: row.x + row.w - side, y: row.y, w: side, h: side };
        let wb = Rect { x: mb.x - side, ..mb };
        let ab = Rect { x: wb.x - side, ..wb };
        self.stats.poll();
        let spans = self.stats.spans(&self.buf);
        let stat = cx.shape_spans(&spans, Font::SANS, r.w);
        let gap = 6.0 * cx.ui.scale;
        let stat_left = ab.x - gap - stat.width;
        let sb = Rect { x: stat_left - gap, y: row.y, w: stat.width + gap * 2.0, h: row.h };

        let e = &self.buf;
        let title = e.title();
        let kind = text_kind(e.path.as_deref());
        let text = HeaderText {
            name: &title,
            path: e.path.as_deref(),
            kind: &kind,
            modified: e.modified,
            changed_on_disk: e.changed_on_disk,
            extra: None,
        };
        let name = self.header.paint(cx, r, sb.x, &text, Some(BTN_NAME));
        self.name_hit = e.path.is_some().then_some(name);
        self.header_rect = Some(r);

        let radius = theme::box_radius(cx.ui);
        let hover = |cx: &mut PaintCx, b: Rect| {
            cx.round_rect(hover_box(b, side), radius, theme::HOVER_BG, theme::HOVER_ALPHA, None);
        };
        // `⋯`, pinned to the right end.
        let more_hover = cx.button(mb, BTN_MORE);
        if more_hover {
            hover(cx, mb);
        }
        cx.tooltip(mb, "More options");
        let icon_side = (ch * 0.8).round();
        let icon = Rect {
            x: mb.x + ((mb.w - icon_side) * 0.5).round(),
            y: mb.y + ((mb.h - icon_side) * 0.5).round(),
            w: icon_side,
            h: icon_side,
        };
        Icon::More.paint(cx, icon, if more_hover { theme::GLYPH_HOVER } else { theme::GLYPH });
        self.more_hit = Some(mb);
        // Soft-wrap toggle left of it.
        if cx.button(wb, BTN_WRAP) {
            hover(cx, wb);
        }
        cx.tooltip(wb, "Soft wrap");
        let wb_color = if wrap { cx.chrome.accent } else { theme::GLYPH };
        cx.label("\u{21a9}", Font::MONO, wb.x + (wb.w - cw) * 0.5, top, wb, wb_color);
        // Autosave toggle (a floppy disk: green on, grey off) left of it; a
        // buffer with no file never autosaves, so there it is off and inert.
        let has_file = self.buf.path.is_some();
        if has_file && cx.button(ab, BTN_AUTOSAVE) {
            hover(cx, ab);
        }
        cx.tooltip(ab, self.autosave_tip(cx.cfg));
        let disk = if has_file && self.autosave_on(cx.cfg) { theme::ON } else { theme::DIM };
        paint_floppy(cx, ab, FLOPPY * ch, disk);
        // The statistic (click to cycle), right-aligned against the disk's
        // whole button (not just the glyph), so their hit areas never overlap.
        if cx.button(sb, BTN_STAT) {
            cx.round_rect(sb, radius, theme::HOVER_BG, theme::HOVER_ALPHA, None);
        }
        cx.tooltip(sb, self.stats.tip());
        cx.place(stat, stat_left, top, sb, theme::TEXT_DIM);
    }
}

/// How much larger than the base cell line `ln` draws: its heading level's
/// size under highlighter `h`, else 1.
fn line_scale(states: &mut StateCache, buf: &EditorBuffer, h: Option<&dyn Highlighter>, ln: usize) -> f32 {
    let (Some(h), Some(line)) = (h, buf.lines.get(ln)) else { return 1.0 };
    let state = if h.carries_state() { states.state_in(h, &buf.lines, buf.edits, ln) } else { 0 };
    syntax::heading_scale(h.heading_level(line, state))
}

/// Where line `ln` sits in a fenced block ([`Highlighter::block_row`]);
/// `None` past the buffer's end.
fn block_row_of(
    states: &mut StateCache,
    spans: &mut SpanCache,
    buf: &EditorBuffer,
    h: &dyn Highlighter,
    ln: usize,
) -> Option<BlockRow> {
    let line = buf.lines.get(ln)?;
    let state = if h.carries_state() { states.state_in(h, &buf.lines, buf.edits, ln) } else { 0 };
    let next = spans.line(h, line, state).1;
    h.block_row(line, state, next)
}

/// The body of the fenced block opening on line `open`, as
/// [`EditorView::block_body`] describes.
fn block_text(
    states: &mut StateCache,
    spans: &mut SpanCache,
    buf: &EditorBuffer,
    h: &dyn Highlighter,
    open: usize,
) -> Option<String> {
    let mut role = |ln| block_row_of(states, spans, buf, h, ln);
    if role(open)? != BlockRow::Open {
        return None;
    }
    let mut last = open;
    while role(last) != Some(BlockRow::Close) && matches!(role(last + 1), Some(BlockRow::Body | BlockRow::Close)) {
        last += 1;
    }
    let closed = role(last) == Some(BlockRow::Close);
    let end = if closed { last } else { last + 1 };
    let container = Container::of(&buf.lines[open]);
    let mut rows: Vec<String> = (open + 1..end).map(|i| container.strip(&buf.lines[i])).collect();
    if !closed {
        while rows.last().is_some_and(|r| r.trim().is_empty()) {
            rows.pop();
        }
    }
    let body = rows.join("\n");
    (!body.trim().is_empty()).then_some(body)
}

/// What a fenced block's rows sit in, read from its opening fence: how many
/// quote markers, and its container's indent (the fence's own, or a list
/// item's content column).
#[derive(Clone, Copy)]
struct Container {
    quotes: usize,
    indent: usize,
}

impl Container {
    fn of(fence: &str) -> Self {
        let c: Vec<char> = fence.chars().collect();
        let (quotes, at) = quote_markers(&c, usize::MAX);
        let indent = c[at..].iter().position(|c| matches!(c, '`' | '~')).unwrap_or(0);
        Container { quotes, indent }
    }

    /// A body row's own text: without a trailing `\r`, the quote markers
    /// (each with one optional space), and up to the indent in spaces.
    fn strip(self, line: &str) -> String {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let c: Vec<char> = line.chars().collect();
        let (_, at) = quote_markers(&c, self.quotes);
        let spaces = c[at..].iter().take(self.indent).take_while(|c| **c == ' ').count();
        c[at + spaces..].iter().collect()
    }
}

/// Up to `max` block-quote markers at the start of `c` (`>`, after up to
/// three spaces, with one optional space after it): how many, and the char
/// index past them.
fn quote_markers(c: &[char], max: usize) -> (usize, usize) {
    let (mut n, mut at) = (0, 0);
    while n < max {
        let p = at + c[at..].iter().take(3).take_while(|c| **c == ' ').count();
        if c.get(p) != Some(&'>') {
            break;
        }
        at = p + 1;
        if c.get(at) == Some(&' ') {
            at += 1;
        }
        n += 1;
    }
    (n, at)
}

/// The line opening the fenced block that line `ln` is in.
fn block_open(
    states: &mut StateCache,
    spans: &mut SpanCache,
    buf: &EditorBuffer,
    h: &dyn Highlighter,
    mut ln: usize,
) -> Option<usize> {
    while block_row_of(states, spans, buf, h, ln)? != BlockRow::Open {
        ln = ln.checked_sub(1)?;
    }
    Some(ln)
}

/// A fenced code block's box over the rows `top..bottom` of the editor's
/// `body`: from just left of the text at `text_x` to just right of its last
/// column at `text_end`. An edge whose block goes on past the viewport
/// (`open`: top, bottom) is drawn past the body and clipped, so the box shows
/// open there. Returns the box cut to the body.
fn paint_code_block(
    cx: &mut PaintCx,
    body: Rect,
    (text_x, text_end): (f32, f32),
    (top, bottom): (f32, f32),
    open: (bool, bool),
) -> Rect {
    let line = theme::hairline(cx.ui);
    let radius = theme::box_radius(cx.ui);
    // Half a cell of room around the text: in the gutter's padding on the
    // left, matched on the right, but never inside the last column.
    let pad = (cx.ui.cell_w * 0.5).round();
    let right = body.x + body.w;
    let x0 = (text_x - pad).max(body.x).round();
    let x1 = (right - pad).max(text_end + line).min(right).round();
    let past = radius + 2.0 * line;
    let y0 = if open.0 { top.min(body.y) - past } else { top + line }.round();
    // Past the body's bottom too: the rows end short of it by a partial row.
    let y1 = if open.1 { bottom.max(body.y + body.h) + past } else { bottom - line }.round();
    let r = Rect { x: x0, y: y0, w: (x1 - x0).max(1.0), h: (y1 - y0).max(1.0) };
    let border = Some((line, theme::CODE_BOX_BORDER, theme::CODE_BOX_BORDER_ALPHA));
    cx.with_clip(body, |cx| cx.round_rect(r, radius, theme::CODE_BOX_BG, theme::CODE_BOX_ALPHA, border));
    let (y0, y1) = (y0.max(body.y), y1.min(body.y + body.h));
    Rect { x: x0, y: y0, w: r.w, h: (y1 - y0).max(0.0) }
}

/// A code block's copy button `id`, a row high at the top right of `block`
/// (its box on screen), over the text on a pill of the box's colour so it
/// reads over glyphs; a check mark after a copy.
fn paint_copy_button(cx: &mut PaintCx, block: Rect, top: f32, id: u32, copied: bool) {
    let ch = cx.ui.cell_h;
    let line = theme::hairline(cx.ui);
    let side = ch;
    let b = Rect { x: (block.x + block.w - line * 2.0 - side).round(), y: top.round(), w: side, h: side };
    let pill = hover_box(b, side);
    let radius = theme::box_radius(cx.ui);
    let border = Some((line, theme::CODE_BOX_BORDER, theme::CODE_BOX_BORDER_ALPHA));
    cx.with_clip(block, |cx| {
        let hover = cx.button(b, id);
        cx.tooltip(b, if copied { "Copied" } else { "Copy code" });
        cx.over_text(|cx| {
            // The box's look, layered as the box is (the GPU blends in
            // linear light, so a premixed colour wouldn't match), on an
            // opaque base that hides the glyphs under it.
            cx.round_rect(pill, radius, EDITOR_BG, 1.0, None);
            cx.round_rect(pill, radius, theme::CODE_BOX_BG, theme::CODE_BOX_ALPHA, border);
            if hover {
                cx.round_rect(pill, radius, theme::HOVER_BG, theme::HOVER_ALPHA, None);
            }
            let icon_side = (ch * 0.75).round();
            let icon = Rect {
                x: b.x + ((b.w - icon_side) * 0.5).round(),
                y: b.y + ((b.h - icon_side) * 0.5).round(),
                w: icon_side,
                h: icon_side,
            };
            let color = match (copied, hover) {
                (true, _) => theme::ON,
                (false, true) => theme::GLYPH_HOVER,
                (false, false) => theme::GLYPH,
            };
            if copied { Icon::Check } else { Icon::Copy }.paint(cx, icon, color);
        });
    });
}

/// The floppy-disk icon's side, in rows.
const FLOPPY: f32 = 0.6;

/// A floppy-disk icon `side` px square, centred in `r`: a body in `color` with
/// the shutter and label cut out in the header colour.
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
        let mut body = rect;
        (self.header_rect, self.name_hit, self.more_hit) = (None, None, None);
        if self.header_on {
            let header_h = DocHeader::height(cx.ui).min(rect.h);
            self.paint_header(cx, Rect { h: header_h, ..rect }, wrap);
            body = Rect { y: rect.y + header_h, h: (rect.h - header_h).max(1.0), ..rect };
        }

        let total_lines = self.buf.lines.len();
        let gutter_w = gutter_width(cx.cfg, total_lines, cw);
        // With no gutter, half a cell before the text holds a code block's
        // left edge.
        let inset = if gutter_w > 0.0 { 0.0 } else { (cw * 0.5).round() };
        let text_x = body.x + gutter_w + inset;
        let text_w = (body.w - gutter_w - inset).max(1.0);
        let rows_layout = RowLayout { text_w, cell: (cw, ch), wrap };
        let text_cols = rows_layout.cols(1.0);
        self.layout = Layout { body, text_x, rows: rows_layout };
        let highlighter = self.buf.path.as_deref().and_then(highlight::for_path);
        self.sync_hl_states();
        // Follow the cursor only when it moved (so manual scrolling sticks),
        // fitting lines by their real heights; reset horizontal scroll under
        // wrap; keep vertical scroll in bounds.
        {
            let states = &mut self.hl_states;
            let cursor = self.buf.cursor;
            let hcols = (!wrap).then(|| rows_layout.cols(line_scale(states, &self.buf, highlighter, cursor.0)));
            let height = |b: &EditorBuffer, ln: usize| {
                let scale = line_scale(states, b, highlighter, ln);
                let rows = rows_layout.rows(&b.lines[ln], scale);
                // The cursor's line counts down to the row its caret is on.
                let n = match ln == b.cursor.0 {
                    true => rows.iter().rposition(|(start, _)| *start <= b.cursor.1).map_or(rows.len(), |i| i + 1),
                    false => rows.len(),
                };
                n as f32 * rows_layout.row_h(scale)
            };
            self.buf.follow_cursor(body.h, height, hcols);
        }
        if wrap {
            self.buf.hscroll = 0;
        }
        self.buf.clamp_scroll_bounds();
        self.layout_rows();

        let e = &self.buf;
        let (cursor, sel) = (e.cursor, e.selection());
        // Unwrapped rows hold their whole line, shaped once, drawn shifted left
        // by hscroll and clipped — moving the viewport rather than reshaping.
        // A row's columns are its scale times the base cell wide.
        let col_w = |r: &TextRow| cw * r.scale;
        let hscroll = e.hscroll;
        let hscroll_px = move |r: &TextRow| hscroll as f32 * col_w(r);
        cx.fill(body, EDITOR_BG);

        // Visible text rows, top to bottom.
        let vis: Vec<TextRow> = self.rows.text_rows().collect();

        self.code_blocks.clear();
        self.hover_block = None;
        // The hovered block's copy button: painted last, over the caret.
        let mut copy_button = None;
        // Each visible row's place in a fenced block.
        let roles: Vec<Option<BlockRow>> = match highlighter {
            Some(h) => vis.iter().map(|r| block_row_of(&mut self.hl_states, &mut self.hl_spans, e, h, r.ln)).collect(),
            None => Vec::new(),
        };
        if let Some(h) = highlighter {
            let text_end = text_x + text_cols as f32 * cw;
            // The visible rows of each block: a line's later wrapped rows,
            // and the lines that go on with it until its closing fence.
            let mut i = 0;
            while i < vis.len() {
                let Some(role) = roles[i] else {
                    i += 1;
                    continue;
                };
                let first = i;
                while i + 1 < vis.len()
                    && (vis[i + 1].ln == vis[i].ln
                        || roles[i] != Some(BlockRow::Close)
                            && matches!(roles[i + 1], Some(BlockRow::Body | BlockRow::Close)))
                {
                    i += 1;
                }
                let mut last = i;
                i += 1;
                let open_top = role != BlockRow::Open || vis[first].start != 0;
                let open_bottom = last + 1 == vis.len() && {
                    let r = &vis[last];
                    r.start + r.len < e.lines[r.ln].chars().count()
                        || roles[last] != Some(BlockRow::Close)
                            && matches!(
                                block_row_of(&mut self.hl_states, &mut self.hl_spans, e, h, r.ln + 1),
                                Some(BlockRow::Body | BlockRow::Close)
                            )
                };
                // An unclosed block ends at its last line with text, as its
                // copy does.
                if !open_bottom && roles[last] != Some(BlockRow::Close) {
                    let open = block_open(&mut self.hl_states, &mut self.hl_spans, e, h, vis[first].ln);
                    let container = open.map(|o| Container::of(&e.lines[o]));
                    while last > first
                        && roles[last] == Some(BlockRow::Body)
                        && container.is_some_and(|c| c.strip(&e.lines[vis[last].ln]).trim().is_empty())
                    {
                        last -= 1;
                    }
                }
                let top = vis[first].y;
                let bottom = vis[last].bottom();
                let rect = paint_code_block(cx, body, (text_x, text_end), (top, bottom), (open_top, open_bottom));
                let first_ln = vis[first].ln;
                self.code_blocks.push(ShownBlock { rect, first: first_ln });
                if cx.modal || !cx.hovered(rect) {
                    continue;
                }
                self.hover_block = Some(first_ln);
                let key = (first_ln, e.edits);
                let (open, has_body) = match self.hover_open {
                    Some((k, open, has_body)) if k == key => (open, has_body),
                    _ => {
                        let open = block_open(&mut self.hl_states, &mut self.hl_spans, e, h, first_ln);
                        let has_body = open
                            .is_some_and(|o| block_text(&mut self.hl_states, &mut self.hl_spans, e, h, o).is_some());
                        self.hover_open = Some((key, open, has_body));
                        (open, has_body)
                    }
                };
                let id = open.and_then(|o| u32::try_from(o).ok()).and_then(|o| BTN_COPY_BLOCK.checked_add(o));
                if let (true, Some(open), Some(id)) = (has_body, open, id) {
                    let copied = self.copied.is_some_and(|(o, until)| o == open && Instant::now() < until);
                    copy_button = Some((rect, top, id, copied));
                }
            }
        }

        // The last row may run past the body's bottom: everything drawn over
        // the rows is clipped to the body, as their text is.
        cx.with_clip(body, |cx| {
            if cx.cfg.editor.cursor_line {
                for r in vis.iter().filter(|r| r.ln == cursor.0) {
                    let rect = Rect { x: body.x, y: r.y, w: body.w, h: r.h };
                    cx.fill_alpha(rect, EDITOR_CURSOR_LINE, EDITOR_CURSOR_LINE_ALPHA);
                }
            }
        });
        // Selection, intersected with each row and clipped to the text area so it
        // never paints the gutter or a neighbouring pane.
        if let Some(((sr, sc), (er, ec))) = sel {
            let text_area = Rect { x: text_x, w: body.x + body.w - text_x, ..body };
            cx.with_clip(text_area, |cx| for r in &vis {
                if r.ln < sr || r.ln > er {
                    continue;
                }
                let first = if r.ln == sr { sc } else { 0 }.max(r.start);
                let last = if r.ln == er { ec } else { usize::MAX }.min(r.start + r.len);
                if last > first {
                    let x0 = (text_x + (first - r.start) as f32 * col_w(r) - hscroll_px(r)).max(text_x);
                    let x1 = (text_x + (last - r.start) as f32 * col_w(r) - hscroll_px(r)).min(body.x + body.w);
                    if x1 > x0 {
                        let rect = Rect { x: x0, y: r.y, w: x1 - x0, h: r.h };
                        cx.fill_alpha(rect, cx.chrome.accent, 0.35);
                    }
                }
            });
        }
        // Text, one cache-keyed row per visual row (wrapped sub-rows stay
        // cache-friendly), shaped within the frame's budget.
        let text_clip = Rect { x: text_x, y: body.y, w: body.x + body.w - text_x, h: body.h };
        // Each row's key in the row cache, for its line number's baseline.
        let mut keys = Vec::with_capacity(vis.len());
        for (i, r) in vis.iter().enumerate() {
            let line = &e.lines[r.ln];
            let (spans, styles) = match highlighter {
                Some(h) => {
                    let state = if h.carries_state() {
                        self.hl_states.state_in(h, &e.lines, e.edits, r.ln)
                    } else {
                        0
                    };
                    let hl = self.hl_spans.spans(h, line, state);
                    let block = roles[i].is_some();
                    syntax::styled_row_spans(line, hl, r.start, r.len, EDITOR_FG, block)
                }
                None => (vec![(line.chars().skip(r.start).take(r.len).collect(), EDITOR_FG)], Vec::new()),
            };
            let styles = RowStyles::new(&styles, cw).scaled(r.scale);
            let key = cx.row_key_styled(spans.iter().map(|(s, c)| (s.as_str(), *c)), styles);
            // Wide enough for the whole row, so layout is hscroll-independent.
            let shape_w = (r.len as f32 * cw).max(text_w);
            let left = text_x - hscroll_px(r);
            cx.budgeted_row_styled(key, || spans, styles, left, r.y, shape_w, text_clip, EDITOR_FG);
            keys.push(key);
        }
        // Line numbers, on each logical line's first visual row, at the base
        // size whatever the row's: on a heading's baseline (read from its own
        // shaped row, whose faces set it), so the number reads as that line's.
        if gutter_w > 0.0 {
            let gutter = Rect { x: body.x, y: body.y, w: gutter_w, h: body.h };
            let mut digits = None;
            for (r, key) in vis.iter().zip(&keys).filter(|(r, _)| r.start == 0) {
                let top = match r.scale == 1.0 {
                    true => r.y,
                    false => {
                        let b = *digits.get_or_insert_with(|| cx.mono_baseline());
                        r.y + cx.row_baseline(*key).unwrap_or(b) * r.scale - b
                    }
                };
                let is_cur = r.ln == cursor.0;
                let num = match cx.cfg.editor.line_numbers {
                    LineNumbers::Relative if !is_cur => (cursor.0 as isize - r.ln as isize).unsigned_abs(),
                    _ => r.ln + 1,
                };
                let s = num.to_string();
                let color = if is_cur { EDITOR_GUTTER_CUR } else { EDITOR_GUTTER };
                let left = text_x - cw - s.chars().count() as f32 * cw;
                cx.row(&[(s, color)], left, top, gutter_w, gutter, color);
            }
        }
        // Cursor bar on the cursor line's last visual row that its column reaches.
        if let Some(r) = vis.iter().rev().find(|r| r.ln == cursor.0 && r.start <= cursor.1) {
            let x = text_x + (cursor.1 - r.start) as f32 * col_w(r) - hscroll_px(r);
            if x >= text_x && x < body.x + body.w {
                cx.with_clip(body, |cx| cx.fill_top(Rect { x, y: r.y, w: 2.0 * scale, h: r.h }, cx.chrome.accent, 0.9));
            }
        }
        if !cx.hovered(body) {
            self.dismiss_popup();
        }
        if let Some(lang) = &self.lang {
            let lines = &self.buf.lines;
            // Diagnostics: an underline under each visual row's part of the span.
            let thick = (1.5 * scale).max(1.0);
            for (start, end, color) in lang.underlines(lines) {
                for (i, r) in vis.iter().enumerate() {
                    if r.ln < start.0 || r.ln > end.0 {
                        continue;
                    }
                    // The span's part of this line, cut to this visual row. A line's
                    // last row runs on past its text, so a span there (e.g. a missing
                    // value) still shows.
                    let from = if r.ln == start.0 { start.1 } else { 0 };
                    let to = if r.ln == end.0 { end.1 } else { lines[r.ln].chars().count() };
                    let last_row = vis.get(i + 1).is_none_or(|next| next.ln != r.ln);
                    let row_end = if last_row { usize::MAX } else { r.start + r.len };
                    let (a, b) = (from.max(r.start), to.min(row_end));
                    if b <= a {
                        continue;
                    }
                    let x0 = (text_x + (a - r.start) as f32 * col_w(r) - hscroll_px(r)).max(text_x);
                    let x1 = (text_x + (b - r.start) as f32 * col_w(r) - hscroll_px(r)).min(body.x + body.w);
                    if x1 > x0 {
                        let under = Rect { x: x0, y: r.bottom() - thick, w: x1 - x0, h: thick };
                        cx.with_clip(body, |cx| cx.fill_top(under, color, 0.9));
                    }
                }
            }
            if let Some(popup) = &lang.popup {
                let (line, col) = popup.at;
                let row = vis.iter().find(|r| r.ln == line && r.start <= col && col <= r.start + r.len);
                if let Some(r) = row {
                    let x = text_x + (col - r.start) as f32 * col_w(r) - hscroll_px(r);
                    paint_popup(cx, popup, (x, r.y, r.h), body);
                }
            }
        }
        if let Some((rect, top, id, copied)) = copy_button {
            paint_copy_button(cx, rect, top, id, copied);
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
        if event.is_other_button() {
            return;
        }
        match *event {
            // A press moves the cursor and anchors a potential selection; one only
            // shows once the cursor is dragged away.
            MouseEvent::Down { pos: (x, y), clicks, .. } => {
                self.dismiss_popup();
                self.buf.end_undo_run();
                if let Some(p) = self.pos_at(x, y) {
                    self.buf.cursor = p;
                    self.buf.anchor = Some(p);
                    // A double-click selects the word under the pointer (the
                    // character it is over, not the nearest caret); a triple-click
                    // the line. The release keeps either.
                    if clicks == 2 {
                        let over = self.char_at(x, y).unwrap_or(p);
                        self.buf.select_word_at(over);
                    } else if clicks >= 3 {
                        self.buf.select_line_at(p.0);
                    }
                    cx.redraw();
                }
            }
            MouseEvent::Drag { pos: (x, y), .. } => {
                if let Some(p) = self.pos_at(x, y) {
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
                let at = self.char_at(x, y);
                if self.lang.as_mut().is_some_and(|l| l.pointer(at, Instant::now())) {
                    cx.redraw();
                }
                // A code block's copy button shows while the pointer is over it.
                let block = self.code_blocks.iter().find(|b| rect_contains(b.rect, x, y)).map(|b| b.first);
                if block != self.hover_block {
                    self.hover_block = block;
                    cx.redraw();
                }
            }
            // The hover popup may extend over the next pane: leave it up.
            MouseEvent::Leave { .. } => {
                if self.hover_block.take().is_some() {
                    cx.redraw();
                }
            }
        }
    }

    fn scroll(&mut self, cx: &mut EventCx, _pos: (f32, f32), dx: f32, dy: f32) {
        let (cw, ch) = (cx.ui.cell_w, cx.ui.cell_h);
        self.scroll_accum += dy;
        // The wheel moves the top line by its own row's height: a heading
        // takes as much travel to pass as it is tall, a base row one cell.
        let layout = RowLayout { cell: (cw, ch), ..self.layout.rows };
        let h = self.highlighter();
        self.sync_hl_states();
        let mut top = self.buf.scroll as isize;
        loop {
            let up = self.scroll_accum > 0.0;
            let ln = if up { top - 1 } else { top };
            let scale = usize::try_from(ln).map_or(1.0, |ln| line_scale(&mut self.hl_states, &self.buf, h, ln));
            let step = layout.row_h(scale);
            if self.scroll_accum.abs() < step {
                break;
            }
            self.scroll_accum -= step.copysign(self.scroll_accum);
            top += if up { -1 } else { 1 };
        }
        let lines = self.buf.scroll as isize - top;
        let e = &mut self.buf;
        let mut changed = false;
        if lines != 0 {
            let max = e.lines.len().saturating_sub(1);
            let next = ((e.scroll as isize - lines).max(0) as usize).min(max);
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
        if self.header_rect.is_some_and(|r| rect_contains(r, pos.0, pos.1)) {
            self.open_file_menu(cx, pos, MenuAlign::Left);
        }
    }

    fn button(&mut self, cx: &mut EventCx, id: u32) {
        match id {
            BTN_NAME => {
                if let Some(hit) = self.name_hit {
                    self.open_file_menu(cx, (hit.x, hit.y + hit.h), MenuAlign::Left);
                }
                return;
            }
            BTN_FILE_MENU => {
                if let Some(at) = self.more_anchor() {
                    self.open_file_menu(cx, at, MenuAlign::Right);
                }
                return;
            }
            BTN_MORE => {
                if let Some(at) = self.more_anchor() {
                    let items = self.header_menu(cx.cfg);
                    cx.request(Request::Menu { items, at, align: MenuAlign::Right });
                }
                return;
            }
            id if id >= BTN_COPY_BLOCK => {
                let open = (id - BTN_COPY_BLOCK) as usize;
                let Some(text) = self.block_body(open) else { return };
                cx.set_clipboard_text(text);
                self.copied = Some((open, Instant::now() + COPIED_FOR));
            }
            BTN_WRAP => self.soft_wrap = Some(!self.wrap_on(cx.cfg)),
            BTN_STAT => self.stats.cycle(),
            id if (BTN_SHOW_STAT..BTN_SHOW_STAT + Stat::ALL.len() as u32).contains(&id) => {
                self.stats.show(Stat::ALL[(id - BTN_SHOW_STAT) as usize]);
            }
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
        let copied = self.copied.map(|(_, until)| until);
        self.autosave_deadline(cfg).into_iter().chain(hover).chain(copied).min()
    }

    fn tick(&mut self, cx: &mut EventCx, now: Instant) {
        if self.copied.is_some_and(|(_, until)| now >= until) {
            self.copied = None;
            cx.redraw();
        }
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
        self.header.refresh(Some(&to));
        self.lang = None;
        self.stats.fetch_head(Some(to));
    }

    /// Unsaved edits stay: they are the user's, and saving them is a choice.
    /// The buffer is marked instead, and saving asks first.
    fn changed_on_disk(&mut self, path: &Path) -> bool {
        let ours = self.buf.path.as_deref().is_some_and(|p| crate::languages::same_file(p, path));
        if !ours || !self.buf.disk_changed() {
            return false;
        }
        self.header.refresh(self.buf.path.as_deref());
        if self.buf.modified {
            // Repaints the mark only the first time.
            !std::mem::replace(&mut self.buf.changed_on_disk, true)
        } else {
            let marked = self.buf.changed_on_disk;
            match self.buf.reload() {
                Ok(true) => {
                    self.note_edits();
                    true
                }
                Ok(false) => false,
                // Unreadable as text now: keep what's shown, flagged.
                Err(_) => !marked,
            }
        }
    }

    fn marked_changed_on_disk(&self) -> bool {
        self.buf.changed_on_disk
    }

    fn disk_conflict(&self) -> bool {
        self.buf.conflicted()
    }

    fn reload(&mut self) -> Result<()> {
        self.buf.reload()?;
        self.header.refresh(self.buf.path.as_deref());
        self.note_edits();
        Ok(())
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
    use crate::plugin::MouseButton;
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

    /// Where column `col` of visual row `row` sits, painted with no header
    /// and the default gutter (2 digits + 2 pad = 4 cells).
    fn at(row: usize, col: usize) -> (f32, f32) {
        let text_x = 4.0 * UI.cell_w;
        (text_x + col as f32 * UI.cell_w, row as f32 * UI.cell_h + 1.0)
    }

    fn paint_plain(h: &mut Harness, v: &mut EditorView) {
        h.paint(v, RECT);
    }

    /// `v` with the document header, as a Files surface shows it.
    fn hosted(mut v: EditorView) -> EditorView {
        v.set_header(true);
        v
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
        paint_plain(&mut h, &mut v);
        let plain = h.frame.top.len();
        settle(&mut v);
        paint_plain(&mut h, &mut v);
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
            paint_plain(&mut h, &mut v);
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
        paint_plain(&mut h, &mut v);

        let p = at(0, 4);
        h.cursor = p;
        h.mouse(&mut v, MouseEvent::Move { pos: p });
        assert_eq!(popup_text(&v), None, "nothing before the dwell");
        let due = v.deadline(&h.cfg).expect("a hover is due");
        h.tick(&mut v, due);
        assert_eq!(popup_text(&v), Some(vec!["flagged".to_string()]));
        settle(&mut v);
        assert_eq!(popup_text(&v), Some(vec!["flagged".to_string(), "docs for bad".to_string()]));
        paint_plain(&mut h, &mut v);
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
        paint_plain(&mut h, &mut v);
        settle(&mut v);
        h.mouse(&mut v, MouseEvent::Move { pos: at(0, 1) });
        let due = v.deadline(&h.cfg).unwrap();
        h.tick(&mut v, due);
        settle(&mut v);
        paint_plain(&mut h, &mut v);
        assert!(popup_text(&v).is_some(), "a plain repaint keeps it");
        assert!(!h.frame.overlay_text.is_empty());
        h.modal = true;
        paint_plain(&mut h, &mut v);
        assert_eq!(popup_text(&v), None);
        assert!(h.frame.overlay_text.is_empty(), "nothing paints over the modal");
        assert_eq!(v.deadline(&h.cfg), None, "and it doesn't come back under it");
    }

    #[test]
    fn typing_closes_the_popup() {
        let mut h = Harness::new();
        let mut v = attached("word\n");
        paint_plain(&mut h, &mut v);
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
    fn a_hosted_editor_draws_its_header_with_the_wrap_toggle_above_the_text() {
        let mut v = hosted(view_with("hello"));
        let mut h = Harness::new();
        let frame = h.paint(&mut v, RECT);
        assert!(frame.view_buttons.iter().any(|(_, _, id)| *id == BTN_WRAP));
        assert!(h.texts().iter().any(|t| t == "Text"), "the kind: {:?}", h.texts());
        let header_h = crate::plugins::files::header::DocHeader::height(UI);
        assert_eq!(v.layout.body.y, header_h, "the text starts below the header");
        v.set_header(false);
        paint_plain(&mut h, &mut v);
        assert!(h.frame.view_buttons.is_empty(), "no header, no buttons");
    }

    #[test]
    fn the_statistic_ends_where_the_disk_button_begins_and_cycles_on_click() {
        let mut v = hosted(view_with("one two"));
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
    fn the_headers_file_name_opens_the_file_menu_and_so_does_a_right_click_on_the_header() {
        let path = tmpdir("editor-menu").join("f.txt");
        std::fs::write(&path, "x
").unwrap();
        let mut v = hosted(EditorView::new(EditorBuffer::open(path.clone()).unwrap()));
        let mut h = Harness::new();
        let name = h.paint(&mut v, RECT).view_buttons.iter().find(|b| b.2 == BTN_NAME).expect("a name button").0;
        let target = FileTarget { path, kind: TargetKind::File, origin: Origin::Document };
        let out = h.button(&mut v, BTN_NAME);
        let at = (name.x, name.y + name.h);
        assert_eq!(out.requests, [Request::FileMenu { target: target.clone(), at, align: MenuAlign::Left }]);
        // Right-click anywhere on the header; not on the text.
        let at = (RECT.w - 2.0, 3.0);
        let out = h.context_menu(&mut v, at);
        assert_eq!(out.requests, [Request::FileMenu { target, at, align: MenuAlign::Left }]);
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
    fn the_headers_more_button_lists_every_option_checked_as_it_stands() {
        let path = tmpdir("editor-more").join("f.txt");
        std::fs::write(&path, "x\n").unwrap();
        let mut v = hosted(EditorView::new(EditorBuffer::open(path.clone()).unwrap()));
        let mut h = Harness::new();
        h.cfg.editor.soft_wrap = false;
        (h.cfg.editor.autosave_after, h.cfg.editor.autosave_on_unfocus) = (0, false);
        let buttons = h.paint(&mut v, RECT).view_buttons.clone();
        let more = buttons.iter().find(|b| b.2 == BTN_MORE).expect("a ⋯ button").0;
        let others = buttons.iter().filter(|b| b.2 != BTN_MORE);
        assert!(others.clone().count() >= 3);
        for b in others {
            assert!(b.0.x + b.0.w <= more.x + 0.5, "⋯ is right of every other header button: {b:?}");
        }
        // Below the button, right edges flush, so the menu stays over the
        // editor.
        let anchor = (more.x + more.w, more.y + more.h);
        let menu = |h: &Harness, v: &mut EditorView| match h.button(v, BTN_MORE).requests.as_slice() {
            [Request::Menu { items, at, align }] => {
                assert_eq!((*at, *align), (anchor, MenuAlign::Right));
                items.clone()
            }
            other => panic!("expected one menu request, got {other:?}"),
        };
        let checked = |items: &[MenuItem], id| items.iter().find(|i| i.id == id).and_then(|i| i.checked);
        let items = menu(&h, &mut v);
        assert_eq!(checked(&items, BTN_WRAP), Some(false));
        assert_eq!(checked(&items, BTN_AUTOSAVE), Some(false));
        assert_eq!(checked(&items, BTN_SHOW_STAT), Some(true), "line count shows");
        assert_eq!(checked(&items, BTN_SHOW_STAT + 1), Some(false));
        assert!(items.iter().all(|i| i.id != BTN_SHOW_STAT + 3), "untracked: no changes item");
        assert!(items.iter().any(|i| i.id == BTN_FILE_MENU && i.enabled), "file actions");
        // "File actions…" opens the file menu where the `⋯` menu was.
        let target = FileTarget { path, kind: TargetKind::File, origin: Origin::Document };
        let out = h.button(&mut v, BTN_FILE_MENU);
        assert_eq!(out.requests, [Request::FileMenu { target, at: anchor, align: MenuAlign::Right }]);

        // Choosing an item is the same click as its header icon.
        for id in [BTN_WRAP, BTN_AUTOSAVE, BTN_SHOW_STAT + 2] {
            assert!(h.button(&mut v, id).redraw);
        }
        let items = menu(&h, &mut v);
        assert_eq!(checked(&items, BTN_WRAP), Some(true));
        assert_eq!(checked(&items, BTN_AUTOSAVE), Some(true));
        assert_eq!(checked(&items, BTN_SHOW_STAT), Some(false));
        assert_eq!(checked(&items, BTN_SHOW_STAT + 2), Some(true), "byte count shows");
        assert_eq!(v.stats.shown(), stats::Stat::Bytes);
    }

    #[test]
    fn a_scratch_buffers_more_menu_offers_autosave_disabled() {
        let mut v = hosted(view_with("x"));
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let out = h.button(&mut v, BTN_MORE);
        let [Request::Menu { items, .. }] = out.requests.as_slice() else {
            panic!("expected a menu");
        };
        let autosave = items.iter().find(|i| i.id == BTN_AUTOSAVE).unwrap();
        assert_eq!((autosave.checked, autosave.enabled), (Some(false), false));
        assert!(items.iter().all(|i| i.id != BTN_FILE_MENU), "no file, no file actions");
    }

    #[test]
    fn a_scratch_buffer_or_an_editor_without_a_header_has_no_file_menu() {
        let mut v = hosted(view_with("x"));
        let mut h = Harness::new();
        let frame = h.paint(&mut v, RECT);
        assert!(frame.view_buttons.iter().all(|b| b.2 != BTN_NAME), "no file, no menu");
        assert!(h.context_menu(&mut v, (RECT.w - 2.0, 3.0)).requests.is_empty());
        let path = tmpdir("editor-menu-strip").join("f.txt");
        std::fs::write(&path, "x
").unwrap();
        let mut v = EditorView::new(EditorBuffer::open(path).unwrap());
        paint_plain(&mut h, &mut v);
        assert!(h.context_menu(&mut v, (RECT.w - 2.0, 3.0)).requests.is_empty(), "no header there");
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
    fn any_file_changed_on_disk_reloads_when_unmodified_and_marks_when_modified() {
        let dir = tmpdir("editor-on-disk-any");
        let path = dir.join("notes.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut v = EditorView::new(EditorBuffer::open(path.clone()).unwrap());
        assert!(!v.changed_on_disk(&path), "untouched: nothing to do");
        std::fs::write(&path, "one two\n").unwrap();
        assert!(v.changed_on_disk(&path));
        assert_eq!(v.buffer().lines, ["one two"]);
        assert!(!v.buffer().changed_on_disk && !v.modified());
        assert!(!v.changed_on_disk(&path), "taken once");

        v.buffer_mut().insert_str("my ");
        std::fs::write(&path, "one two three\n").unwrap();
        assert!(v.changed_on_disk(&path), "the mark appears");
        assert_eq!(v.buffer().lines, ["my one two"], "unsaved edits stay");
        assert!(v.buffer().changed_on_disk && v.modified());
        assert!(v.marked_changed_on_disk(), "the tab strip shows it too");
        assert!(!v.changed_on_disk(&path), "marked once");
        assert!(v.disk_conflict(), "and still a conflict");
    }

    #[test]
    fn a_file_that_vanishes_is_neither_reloaded_nor_marked() {
        let path = tmpdir("editor-on-disk-gone").join("f.txt");
        std::fs::write(&path, "a\n").unwrap();
        let mut v = EditorView::new(EditorBuffer::open(path.clone()).unwrap());
        v.buffer_mut().insert_str("b");
        std::fs::remove_file(&path).unwrap();
        assert!(!v.changed_on_disk(&path));
        assert!(!v.buffer().changed_on_disk && !v.disk_conflict());
        assert_eq!(v.buffer().lines, ["ba"]);
    }

    #[test]
    fn a_scratch_buffer_shows_autosave_off_and_inert() {
        let mut v = hosted(view_with("x"));
        let mut h = Harness::new();
        let frame = h.paint(&mut v, RECT);
        assert!(frame.view_buttons.iter().all(|b| b.2 != BTN_AUTOSAVE), "no toggle to click");
        assert_eq!(v.autosave_tip(&Config::default()), "Autosave off (no file)");
    }

    #[test]
    fn hovering_the_wrap_toggle_offers_a_tooltip() {
        let mut v = hosted(view_with("x"));
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
        paint_plain(&mut h, &mut v);
        h.mouse(&mut v, MouseEvent::Down { pos: at(1, 3), clicks: 1, button: MouseButton::Left });
        assert_eq!(v.buffer().cursor, (1, 3));
        h.mouse(&mut v, MouseEvent::Drag { pos: at(0, 5), button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Up { pos: at(0, 5), dragged: true, button: MouseButton::Left });
        assert_eq!(v.buffer().selected_text().as_deref(), Some(" world\nsec"));
        // A plain click clears it.
        h.mouse(&mut v, MouseEvent::Down { pos: at(0, 1), clicks: 1, button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Up { pos: at(0, 1), dragged: false, button: MouseButton::Left });
        assert_eq!(v.buffer().selection(), None);
    }

    #[test]
    fn double_click_selects_the_word_and_keeps_it_on_release() {
        let mut v = view_with("hello world");
        let mut h = Harness::new();
        paint_plain(&mut h, &mut v);
        h.mouse(&mut v, MouseEvent::Down { pos: at(0, 7), clicks: 2, button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Up { pos: at(0, 7), dragged: false, button: MouseButton::Left });
        assert_eq!(v.buffer().selected_text().as_deref(), Some("world"));
    }

    #[test]
    fn double_click_on_the_right_half_of_a_cell_selects_that_cells_word() {
        let mut v = view_with("hello world again");
        let mut h = Harness::new();
        paint_plain(&mut h, &mut v);
        let (x, y) = at(0, 10); // the `d` ending "world"
        let pos = (x + UI.cell_w * 0.75, y);
        h.mouse(&mut v, MouseEvent::Down { pos, clicks: 2, button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Up { pos, dragged: false, button: MouseButton::Left });
        assert_eq!(v.buffer().selected_text().as_deref(), Some("world"));
    }

    #[test]
    fn triple_click_selects_the_line() {
        let mut v = view_with("one two\nthree");
        let mut h = Harness::new();
        paint_plain(&mut h, &mut v);
        h.mouse(&mut v, MouseEvent::Down { pos: at(0, 5), clicks: 3, button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Up { pos: at(0, 5), dragged: false, button: MouseButton::Left });
        assert_eq!(v.buffer().selected_text().as_deref(), Some("one two"));
    }

    #[test]
    fn a_click_past_a_short_scrolled_line_lands_at_its_end() {
        let mut v = view_with(&format!("ab\n{}", "x".repeat(100)));
        v.soft_wrap = Some(false);
        v.buf.hscroll = 50;
        let mut h = Harness::new();
        paint_plain(&mut h, &mut v);
        h.mouse(&mut v, MouseEvent::Down { pos: at(0, 3), clicks: 1, button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Up { pos: at(0, 3), dragged: false, button: MouseButton::Left });
        assert_eq!(v.buffer().cursor, (0, 2));
        h.key(&mut v, press(Key::Backspace));
        assert_eq!(v.buffer().lines[0], "a");
    }

    #[test]
    fn typing_after_a_mouse_selection_is_its_own_undo_step() {
        let mut v = view_with("");
        let mut h = Harness::new();
        paint_plain(&mut h, &mut v);
        for c in "abc".chars() {
            h.key(&mut v, press(Key::Char(c)));
        }
        h.mouse(&mut v, MouseEvent::Down { pos: at(0, 0), clicks: 1, button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Drag { pos: at(0, 3), button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Up { pos: at(0, 3), dragged: true, button: MouseButton::Left });
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
        paint_plain(&mut h, &mut v);
        h.mouse(&mut v, MouseEvent::Down { pos: at(1, 2), clicks: 1, button: MouseButton::Left });
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

    /// An editor with unsaved text "x" whose file another program then
    /// rewrote.
    fn conflicted(name: &str) -> (EditorView, PathBuf) {
        let path = tmpdir(name).join("f.txt");
        std::fs::write(&path, "disk\n").unwrap();
        let mut v = EditorView::new(EditorBuffer::open(path.clone()).unwrap());
        v.buffer_mut().insert_str("x");
        std::fs::write(&path, "someone else's\n").unwrap();
        (v, path)
    }

    #[test]
    fn cmd_s_over_a_file_changed_on_disk_asks_instead_of_writing() {
        let (mut v, path) = conflicted("editor-save-conflict");
        let h = Harness::new();
        let (used, out) = h.key(&mut v, cmd(Key::Char('s')));
        assert!(used);
        assert_eq!(out.requests, vec![Request::SaveConflict]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "someone else's\n");
        assert!(v.modified());
    }

    #[test]
    fn autosave_skips_a_file_changed_on_disk_and_keeps_the_edits() {
        let (mut v, path) = conflicted("editor-autosave-conflict");
        let h = Harness::new();
        let out = h.focus(&mut v, false);
        assert!(out.requests.is_empty(), "no save on blur");
        assert_eq!(v.save_if_modified(), None);
        assert_eq!(v.autosave(&h.cfg), None, "nor on quit or app blur");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "someone else's\n");
        assert!(v.modified() && v.buffer().changed_on_disk, "still unsaved, and marked");
    }

    #[test]
    fn undoing_past_a_reload_conflicts_with_the_file_it_never_saw() {
        let dir = tmpdir("editor-undo-reload");
        let path = dir.join("f.txt");
        std::fs::write(&path, "old\n").unwrap();
        let mut v = EditorView::new(EditorBuffer::open(path.clone()).unwrap());
        std::fs::write(&path, "new\n").unwrap();
        assert!(v.changed_on_disk(&path));
        assert_eq!(v.buffer().lines, ["new"]);
        v.buffer_mut().undo();
        assert_eq!(v.buffer().lines, ["old"]);
        assert!(v.modified() && v.buffer().changed_on_disk && v.disk_conflict());
        let h = Harness::new();
        assert_eq!(v.save_if_modified(), None, "autosave skips it");
        assert_eq!(v.autosave(&h.cfg), None);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        let (_, out) = h.key(&mut v, cmd(Key::Char('s')));
        assert_eq!(out.requests, vec![Request::SaveConflict], "an explicit save asks");
        // Redoing back to the reloaded text is the file again.
        v.buffer_mut().redo();
        assert!(!v.modified() && !v.disk_conflict());
        assert!(!v.marked_changed_on_disk(), "no longer marked");
    }

    #[test]
    fn undo_keeps_the_mark_while_the_file_changed_under_unsaved_edits() {
        let (mut v, path) = conflicted("editor-undo-conflict");
        assert!(v.changed_on_disk(&path), "the mark appears");
        v.buffer_mut().undo();
        assert!(v.marked_changed_on_disk() && v.disk_conflict(), "the file still isn't what the buffer read");
        v.buffer_mut().redo();
        assert!(v.marked_changed_on_disk());
    }

    #[test]
    fn a_file_that_turns_unreadable_is_checked_once_and_blocks_autosave() {
        let dir = tmpdir("editor-unreadable");
        let path = dir.join("f.txt");
        std::fs::write(&path, "text\n").unwrap();
        let mut v = EditorView::new(EditorBuffer::open(path.clone()).unwrap());
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        assert!(v.changed_on_disk(&path), "the mark appears");
        assert_eq!(v.buffer().lines, ["text"], "the text is kept");
        assert!(v.buffer().changed_on_disk && !v.modified());
        assert!(!v.buffer().disk_changed(), "the new version counts as seen");
        assert!(!v.changed_on_disk(&path), "and is not read again");
        v.buffer_mut().insert_str("x");
        assert_eq!(v.save_if_modified(), None, "autosave would write over it");
        assert_eq!(std::fs::read(&path).unwrap(), [0xff, 0xfe, 0x00]);
    }

    #[test]
    fn saving_a_file_deleted_on_disk_recreates_it_without_asking() {
        let (mut v, path) = conflicted("editor-save-deleted");
        std::fs::remove_file(&path).unwrap();
        let h = Harness::new();
        let (_, out) = h.key(&mut v, cmd(Key::Char('s')));
        assert_eq!(out.requests, vec![Request::Saved(path.clone())]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "xdisk\n");
        assert!(!v.disk_conflict(), "the recreated file is ours");
    }

    #[test]
    fn a_save_is_not_a_conflict_with_itself() {
        let (mut v, path) = conflicted("editor-save-twice");
        v.reload().unwrap();
        v.buffer_mut().insert_str("y");
        let h = Harness::new();
        for _ in 0..2 {
            let (_, out) = h.key(&mut v, cmd(Key::Char('s')));
            assert_eq!(out.requests, vec![Request::Saved(path.clone())]);
        }
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
        paint_plain(&mut h, &mut md);
        let md_quads = bg_rects(&h).len();
        let row0 = text_item(&h, 0);
        let mut txt = view_with(text);
        paint_plain(&mut h, &mut txt);
        assert_eq!(bg_rects(&h).len(), md_quads, "no decoration quads for plain rows");
        // The first row has one span in the plain colour: the key a plain row always had.
        let key = h.text.row_cache.row_key(std::iter::once(("hello world", EDITOR_FG)));
        assert_eq!(row0, TextSrc::Row(key));
        paint_plain(&mut h, &mut txt);
        assert_eq!(text_item(&h, 0), TextSrc::Row(key), "same as with no highlighter");
    }

    #[test]
    fn markdown_styles_draw_underline_and_code_box_quads_and_keep_the_markers() {
        let mut h = Harness::new();
        let mut plain = markdown_view("editor-md-plain2", "see the docs now");
        paint_plain(&mut h, &mut plain);
        let base = bg_rects(&h);

        let mut v = markdown_view("editor-md-link", "see [the docs](u) now");
        paint_plain(&mut h, &mut v);
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
        paint_plain(&mut h, &mut v);
        let boxes: Vec<_> = bg_rects(&h).into_iter().filter(|r| !base.contains(r) && r.3 > 3.0).collect();
        assert_eq!(boxes.len(), 1, "{boxes:?}");
        // Around the code text, padded a little into the backticks' cells.
        assert_eq!((boxes[0].0, boxes[0].2), (text_x + 3.0 * cw - 2.0, 4.0 * cw + 4.0));
    }

    /// The rounded, outlined boxes of a painted frame (code boxes) as pixel
    /// rects `(x, y, w, h)`.
    fn code_boxes(h: &Harness) -> Vec<(f32, f32, f32, f32)> {
        let (sw, sh) = h.frame.screen;
        h.frame
            .bg
            .iter()
            .filter(|q| q.radius > 0.0 && q.border_w > 0.0 && q.px[1] > UI.cell_h / 2.0)
            .map(|q| (((q.pos[0] + 1.0) / 2.0 * sw).round(), ((1.0 - q.pos[1]) / 2.0 * sh).round(), q.px[0], q.px[1]))
            .collect()
    }

    #[test]
    fn a_fenced_block_is_one_box_over_its_rows_with_its_language_colours() {
        let mut h = Harness::new();
        let mut v = markdown_view("editor-md-fence", "text\n```toml\nname = 1\n```\nafter `x`");
        paint_plain(&mut h, &mut v);
        let (text_x, ch, line) = (4.0 * UI.cell_w, UI.cell_h, theme::hairline(UI));
        let boxes = code_boxes(&h);
        // The block (rows 1..=3, fences included) and the inline span on row 4.
        assert_eq!(boxes.len(), 2, "{boxes:?}");
        let pad = (UI.cell_w * 0.5).round();
        // The text's 46 columns reach the right edge, so the box does too.
        assert_eq!(boxes[0], (text_x - pad, ch + line, RECT.w - text_x + pad, 3.0 * ch - 2.0 * line));
        assert!(boxes[1].1 >= 4.0 * ch, "the inline box is on the prose row: {boxes:?}");
        // The body is coloured as TOML (a key, an operator, a number), not as one run of code.
        let toml_row = [("name ", syntax::color(highlight::Token::Key, EDITOR_FG)), ("= ", EDITOR_FG),
            ("1", syntax::color(highlight::Token::Number, EDITOR_FG))];
        let key = h.text.row_cache.row_key(toml_row.into_iter());
        assert_eq!(text_item(&h, 2), TextSrc::Row(key));
    }

    #[test]
    fn adjacent_blocks_and_wrapped_rows_box_by_block() {
        let mut h = Harness::new();
        let long = "x".repeat(60);
        let mut v = markdown_view("editor-md-fences", &format!("```\n{long}\n```\n```\nb\n```"));
        v.soft_wrap = Some(true);
        paint_plain(&mut h, &mut v);
        let ch = UI.cell_h;
        let rows: Vec<(f32, f32)> = code_boxes(&h).iter().map(|b| ((b.1 / ch).floor(), (b.3 / ch).round())).collect();
        // The 60-char line wraps onto two rows (46 columns fit), inside the first box.
        assert_eq!(rows, [(0.0, 4.0), (4.0, 3.0)], "two boxes, wrapped rows inside the first");
    }

    #[test]
    fn a_block_past_the_viewport_is_open_at_that_edge() {
        let mut h = Harness::new();
        let body: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
        let mut v = markdown_view("editor-md-fence-open", &format!("text\n```\n{}\n```\n", body.join("\n")));
        // Starts on screen and runs past its bottom.
        paint_plain(&mut h, &mut v);
        let b = code_boxes(&h)[0];
        assert!(b.1 > UI.cell_h && b.1 + b.3 > RECT.h, "closed top, open bottom: {b:?}");
        // Scrolled into its middle: open at both edges.
        v.buf.scroll = 10;
        v.buf.cursor = (20, 0);
        paint_plain(&mut h, &mut v);
        let b = code_boxes(&h)[0];
        assert!(b.1 < 0.0 && b.1 + b.3 > RECT.h, "open at both edges: {b:?}");
        assert_eq!(h.frame.bg.iter().find(|q| q.border_w > 0.0).unwrap().clip, [0.0, 0.0, RECT.w, RECT.h]);
        // Scrolled to its end: open top, closed bottom.
        v.buf.scroll = 35;
        v.buf.cursor = (40, 0);
        paint_plain(&mut h, &mut v);
        let b = code_boxes(&h)[0];
        assert!(b.1 < 0.0 && b.1 + b.3 < 8.0 * UI.cell_h, "open top, closed at its fence: {b:?}");
    }

    /// The copy buttons painted, as (hit rect, id).
    fn copy_buttons(h: &Harness) -> Vec<(Rect, u32)> {
        h.frame.view_buttons.iter().filter(|b| b.2 >= BTN_COPY_BLOCK).map(|b| (b.0, b.2)).collect()
    }

    /// Paint `v` with the pointer at `at`, and copy with the button there.
    fn copy_at(h: &mut Harness, v: &mut EditorView, rect: Rect, at: (f32, f32)) -> Option<String> {
        h.cursor = at;
        h.paint(v, rect);
        let (_, id) = *copy_buttons(h).first()?;
        *h.clipboard.borrow_mut() = None;
        h.button(v, id);
        h.clipboard.borrow().clone()
    }

    #[test]
    fn a_blocks_copy_button_shows_on_hover_over_the_text_and_copies_its_body() {
        let text = "text\n```toml\nname = 1\n\nb = 2\n```\n- ```\n  a\n    b\n  ```\n> ```py\n> >>> 1+1\n> ```\n  ```\n      x\n  ```\n```\nopen\n\n";
        let mut v = markdown_view("editor-md-copy", text);
        let mut h = Harness::new();
        let tall = Rect { h: 400.0, ..RECT };
        h.paint(&mut v, tall);
        assert!(copy_buttons(&h).is_empty(), "no button until the pointer is over a block");
        // Over the first block's body: its button, on the opening fence's
        // row, keyed on that line, drawn over the text.
        h.cursor = at(3, 2);
        h.paint(&mut v, tall);
        let buttons = copy_buttons(&h);
        assert_eq!(buttons.len(), 1);
        let (b, id) = buttons[0];
        // Its hit rect is cut to the box, which starts a hairline into the row.
        assert_eq!((b.y, id), (UI.cell_h + theme::hairline(UI), BTN_COPY_BLOCK + 1));
        assert!(b.x + b.w <= RECT.w, "inside the box");
        assert_eq!(pill_colors(&h).len(), 2, "the button's pill is drawn over the text");
        // Each block's body: no fences, the quote's own marker only, the
        // list item's content column, the fence's indent (the rest kept),
        // and an unclosed block without its trailing blank rows.
        for (row, want) in [(2, "name = 1\n\nb = 2"), (7, "a\n  b"), (11, ">>> 1+1"), (14, "    x"), (17, "open")] {
            assert_eq!(copy_at(&mut h, &mut v, tall, at(row, 2)).as_deref(), Some(want), "row {row}");
        }
        // The unclosed block's box ends at its text.
        h.paint(&mut v, tall);
        let last = *code_boxes(&h).last().unwrap();
        assert_eq!(((last.1 + last.3) / UI.cell_h).round(), 18.0, "{last:?}");
        // Hovered again after scrolling: the same id.
        v.buf.scroll = 3;
        v.buf.cursor = (3, 0);
        h.cursor = at(0, 2);
        h.paint(&mut v, tall);
        assert_eq!(copy_buttons(&h)[0].1, id);
        // The button is gone once the pointer leaves: a click there is the text's.
        h.cursor = (-1.0, -1.0);
        h.paint(&mut v, tall);
        assert!(copy_buttons(&h).is_empty());
    }

    /// The over-text quads in the copy button pill's two layers: the
    /// editor's background, opaque, then the code box's fill.
    fn pill_colors(h: &Harness) -> Vec<([f32; 4], f32)> {
        use crate::plugin::paint::round_quad;
        let r = Rect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 };
        let base = round_quad(r, 1.0, 1.0, 0.0, EDITOR_BG, 1.0, None).color;
        let fill = round_quad(r, 1.0, 1.0, 0.0, theme::CODE_BOX_BG, theme::CODE_BOX_ALPHA, None).color;
        h.frame.top.iter().filter(|q| q.color == base || q.color == fill).map(|q| (q.color, q.border_w)).collect()
    }

    #[test]
    fn the_copy_pill_layers_the_box_look_over_an_opaque_base_after_the_caret() {
        let mut v = markdown_view("editor-md-pill", "```\na\n```");
        let mut h = Harness::new();
        paint_plain(&mut h, &mut v);
        assert!(pill_colors(&h).is_empty(), "no pill while the pointer is away");
        h.cursor = at(1, 0);
        h.paint(&mut v, RECT);
        let layers = pill_colors(&h);
        assert_eq!(layers.len(), 2, "{layers:?}");
        assert_eq!(layers[0].0[3], 1.0, "an opaque base");
        assert_eq!(layers[1].0[3], theme::CODE_BOX_ALPHA, "then the box's fill");
        assert!(layers[1].1 > 0.0, "with the box's border");
        // The caret (the cursor is at the start of line 0) is drawn first.
        let caret = h.frame.top.iter().position(|q| q.px[0] == 2.0 * UI.scale).expect("a caret");
        let pill = h.frame.top.iter().position(|q| q.color[3] == 1.0 && q.radius > 0.0).unwrap();
        assert!(caret < pill, "the button covers the caret");
    }

    #[test]
    fn a_blank_bodied_or_quoted_unclosed_block_trims_as_its_copy_does() {
        let mut h = Harness::new();
        let mut v = markdown_view("editor-md-blank-body", "```\n  \n```\n");
        h.cursor = at(1, 0);
        h.paint(&mut v, RECT);
        assert!(copy_buttons(&h).is_empty(), "a body of blank rows has nothing to copy");
        // An unclosed quoted block ends at its last row with text, `>` aside.
        let mut v = markdown_view("editor-md-quoted-open", "> ```\n> a\n>\n> \nafter");
        paint_plain(&mut h, &mut v);
        let b = code_boxes(&h)[0];
        assert_eq!(((b.1 + b.3) / UI.cell_h).round(), 2.0, "{b:?}");
        assert_eq!(copy_at(&mut h, &mut v, RECT, at(1, 3)).as_deref(), Some("a"));
    }

    #[test]
    fn a_move_compares_with_the_block_last_painted_as_hovered() {
        let mut v = markdown_view("editor-md-hover-painted", "```\na\n```\nafter");
        let mut h = Harness::new();
        h.cursor = at(1, 0);
        h.paint(&mut v, RECT);
        assert!(!h.mouse(&mut v, MouseEvent::Move { pos: at(1, 1) }).redraw, "already shown");
        h.cursor = (-1.0, -1.0);
        h.paint(&mut v, RECT);
        assert!(h.mouse(&mut v, MouseEvent::Move { pos: at(1, 1) }).redraw, "not shown yet");
    }

    #[test]
    fn copying_shows_a_check_mark_until_its_deadline() {
        let mut v = markdown_view("editor-md-copied", "```\na\n```");
        let mut h = Harness::new();
        h.cursor = at(1, 0);
        h.paint(&mut v, RECT);
        let quads = |h: &Harness| h.frame.top.len();
        let copy = quads(&h);
        let (_, id) = copy_buttons(&h)[0];
        let out = h.button(&mut v, id);
        assert!(out.redraw);
        assert_eq!(h.clipboard.borrow().as_deref(), Some("a"));
        h.paint(&mut v, RECT);
        assert_ne!(quads(&h), copy, "the check mark replaces the copy icon");
        let due = v.deadline(&h.cfg).expect("a deadline to put the icon back");
        assert!(h.tick(&mut v, due).redraw);
        assert_eq!(v.deadline(&h.cfg), None);
        h.paint(&mut v, RECT);
        assert_eq!(quads(&h), copy);
    }

    #[test]
    fn an_empty_block_has_no_copy_button_and_crlf_rows_copy_without_cr() {
        let mut h = Harness::new();
        let mut v = markdown_view("editor-md-copy-empty", "```\n```\n");
        h.cursor = at(0, 1);
        h.paint(&mut v, RECT);
        assert!(copy_buttons(&h).is_empty());
        *h.clipboard.borrow_mut() = Some("kept".into());
        h.button(&mut v, BTN_COPY_BLOCK);
        assert_eq!(h.clipboard.borrow().as_deref(), Some("kept"), "nothing to copy");

        let mut v = markdown_view("editor-md-copy-crlf", "```\r\na\r\nb\r\n```\r\n");
        assert_eq!(copy_at(&mut h, &mut v, RECT, at(1, 0)).as_deref(), Some("a\nb"));
    }

    #[test]
    fn hovering_into_and_out_of_a_block_repaints() {
        let mut v = markdown_view("editor-md-hover", "text\n```\na\n```\nafter");
        let mut h = Harness::new();
        paint_plain(&mut h, &mut v);
        assert!(h.mouse(&mut v, MouseEvent::Move { pos: at(2, 0) }).redraw, "into the block");
        assert!(!h.mouse(&mut v, MouseEvent::Move { pos: at(2, 1) }).redraw, "within it");
        assert!(h.mouse(&mut v, MouseEvent::Move { pos: at(4, 0) }).redraw, "out of it");
        h.mouse(&mut v, MouseEvent::Move { pos: at(2, 0) });
        assert!(h.mouse(&mut v, MouseEvent::Leave { pos: (-1.0, -1.0) }).redraw, "leaving the editor");
    }

    #[test]
    fn with_line_numbers_off_a_blocks_left_edge_shows() {
        let mut h = Harness::new();
        h.cfg.editor.line_numbers = LineNumbers::Off;
        let mut v = markdown_view("editor-md-no-gutter", "```\na\n```");
        paint_plain(&mut h, &mut v);
        let pad = (UI.cell_w * 0.5).round();
        assert_eq!(v.layout.text_x, pad, "the text leaves room for the box's edge");
        let quad = h.frame.bg.iter().find(|q| q.border_w > 0.0).unwrap();
        let x = ((quad.pos[0] + 1.0) / 2.0 * h.frame.screen.0).round();
        assert_eq!(x, 0.0);
        assert!(quad.clip[0] <= x, "its left edge is inside the clip");
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
    fn a_comment_in_a_code_block_without_a_highlighter_uses_the_language_table() {
        let dir = tmpdir("editor-comment-rust-fence");
        let path = dir.join("a.md");
        std::fs::write(&path, "```rust\nlet a = 1;\n\nlet b = 2;\n```\n").unwrap();
        let mut v = EditorView::new(EditorBuffer::open(path).unwrap());
        v.buf.anchor = Some((1, 0));
        v.buf.cursor = (3, 3);
        v.toggle_comment().unwrap();
        assert_eq!(v.buffer().lines[1..4], ["// let a = 1;", "", "// let b = 2;"]);
        v.buf.anchor = None;
        v.buf.cursor = (0, 0);
        v.toggle_comment().unwrap();
        assert_eq!(v.buffer().lines[0], "<!-- ```rust -->", "the opening fence is Markdown");
    }

    #[test]
    fn a_comment_in_a_quoted_code_block_stays_in_the_quote() {
        let dir = tmpdir("editor-comment-quoted-fence");
        let path = dir.join("a.md");
        let text = "> ```rust\n> let a = 1;\n>\n> \n>     b();\n> ```\n- ```python\n  >>> c = 1\n  ```\n";
        std::fs::write(&path, text).unwrap();
        let mut v = EditorView::new(EditorBuffer::open(path).unwrap());
        // A bare `>` row is blank: left alone, and no limit on the depth.
        v.buf.anchor = Some((1, 0));
        v.buf.cursor = (4, 4);
        v.toggle_comment().unwrap();
        assert_eq!(v.buffer().lines[1..5], ["> // let a = 1;", ">", "> ", "> //     b();"]);
        v.toggle_comment().unwrap();
        assert_eq!(v.buffer().lines[1..5], ["> let a = 1;", ">", "> ", ">     b();"]);
        // A list item's body comments at its indent, even before a `>`.
        v.buf.anchor = None;
        v.buf.cursor = (7, 0);
        v.toggle_comment().unwrap();
        assert_eq!(v.buffer().lines[7], "  # >>> c = 1");
        v.toggle_comment().unwrap();
        assert_eq!(v.buffer().lines.join("\n") + "\n", text);
    }

    #[test]
    fn a_comment_in_a_quoted_list_items_code_block_stays_in_the_quote() {
        let dir = tmpdir("editor-comment-quoted-item");
        let path = dir.join("a.md");
        let text = "> - ```rust\n>   a();\n>\n>   b();\n>   ```\n";
        std::fs::write(&path, text).unwrap();
        let mut v = EditorView::new(EditorBuffer::open(path).unwrap());
        v.buf.anchor = Some((1, 0));
        v.buf.cursor = (3, 3);
        v.toggle_comment().unwrap();
        assert_eq!(v.buffer().lines[1..4], [">   // a();", ">", ">   // b();"]);
        v.toggle_comment().unwrap();
        assert_eq!(v.buffer().lines.join("\n") + "\n", text);
    }

    #[test]
    fn a_selection_leaving_a_code_block_uses_markdowns_comments() {
        let dir = tmpdir("editor-comment-fence-edge");
        let path = dir.join("a.md");
        let text = "```toml\na = 1\n```\ntext\n```toml\nb = 2\n```\n";
        std::fs::write(&path, text).unwrap();
        let mut v = EditorView::new(EditorBuffer::open(path).unwrap());
        let lines = |v: &EditorView| v.buffer().lines[..7].to_vec();
        // Over the closing fence.
        v.buf.anchor = Some((1, 0));
        v.buf.cursor = (3, 4);
        v.toggle_comment().unwrap();
        assert_eq!(lines(&v)[1..4], ["<!-- a = 1", "```", "text -->"]);
        v.toggle_comment().unwrap();
        // Across two blocks of one language, through the text between them.
        v.buf.anchor = Some((1, 0));
        v.buf.cursor = (5, 5);
        v.toggle_comment().unwrap();
        assert_eq!(lines(&v)[1], "<!-- a = 1");
        assert_eq!(lines(&v)[5], "b = 2 -->");
        // One undo step restores it.
        v.buffer_mut().undo();
        assert_eq!(lines(&v).join("\n") + "\n", text);
    }

    #[test]
    fn a_file_in_a_language_without_a_highlighter_toggles_its_comments() {
        let dir = tmpdir("editor-comment-table");
        for (name, text, want) in [("a.rs", "let a = 1;", "// let a = 1;"), ("a.py", "a = 1", "# a = 1")] {
            let path = dir.join(name);
            std::fs::write(&path, format!("{text}\n")).unwrap();
            let mut v = EditorView::new(EditorBuffer::open(path).unwrap());
            v.toggle_comment().unwrap();
            assert_eq!(v.buffer().lines[0], want, "{name}");
            v.toggle_comment().unwrap();
            assert_eq!(v.buffer().lines[0], text, "{name}");
        }
        let json = dir.join("a.json");
        std::fs::write(&json, "{}\n").unwrap();
        let mut v = EditorView::new(EditorBuffer::open(json).unwrap());
        assert!(v.toggle_comment().is_err(), "JSON has no comments");
        assert_eq!(v.buffer().lines[0], "{}");
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
        paint_plain(&mut h, &mut v);
        check(&mut v);
        // Delete the opening fence's first backtick: the block is no longer a fence.
        v.buf.cursor = (1, 0);
        h.key(&mut v, press(Key::Delete));
        paint_plain(&mut h, &mut v);
        check(&mut v);
        // Type it back, then undo.
        h.key(&mut v, press(Key::Char('`')));
        paint_plain(&mut h, &mut v);
        check(&mut v);
        h.key(&mut v, cmd(Key::Char('z')));
        paint_plain(&mut h, &mut v);
        check(&mut v);
        // A new line above shifts the fence down.
        v.buf.cursor = (0, 1);
        h.key(&mut v, press(Key::Enter));
        paint_plain(&mut h, &mut v);
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

    /// A view of `text` saved as `name` (its extension picks the highlighter).
    fn file_view(tag: &str, name: &str, text: &str) -> EditorView {
        let path = tmpdir(tag).join(name);
        std::fs::write(&path, text).unwrap();
        EditorView::new(EditorBuffer::open(path).unwrap())
    }

    /// The text column's items as `(top, scale)`, top to bottom.
    fn text_rows(h: &Harness) -> Vec<(f32, f32)> {
        let text_x = 4.0 * UI.cell_w;
        h.frame.text.iter().filter(|t| t.left == text_x).map(|t| (t.top, t.scale)).collect()
    }

    /// Quads as unrounded pixel rects `(x, y, w, h)`.
    fn rects(h: &Harness, quads: &[crate::plugin::paint::QuadInstance]) -> Vec<(f32, f32, f32, f32)> {
        let (sw, sh) = h.frame.screen;
        quads
            .iter()
            .map(|q| ((q.pos[0] + 1.0) / 2.0 * sw, (1.0 - q.pos[1]) / 2.0 * sh, q.size[0] / 2.0 * sw, -q.size[1] / 2.0 * sh))
            .collect()
    }

    fn near(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
        [a.0 - b.0, a.1 - b.1, a.2 - b.2, a.3 - b.3].iter().all(|d| d.abs() < 0.01)
    }

    const H1: f32 = 1.8;
    const H2: f32 = 1.5;
    /// The rows' heights at H1, H2 and H3: whole pixels.
    const R1: f32 = 29.0;
    const R2: f32 = 24.0;
    const R3: f32 = 20.0;

    #[test]
    fn heading_rows_draw_larger_and_push_the_rows_below_down() {
        let mut h = Harness::new();
        let mut v = markdown_view("editor-md-sized", "# Title\n## Sub\nbody\n### Three\n##### Five");
        paint_plain(&mut h, &mut v);
        let y2 = R1 + R2;
        assert_eq!(
            text_rows(&h),
            vec![(0.0, H1), (R1, H2), (y2, 1.0), (y2 + UI.cell_h, 1.25), (y2 + UI.cell_h + R3, 1.0)],
            "H5 stays at body size"
        );
    }

    #[test]
    fn a_line_number_beside_a_heading_is_base_size_on_its_baseline() {
        let mut h = Harness::new();
        let mut v = markdown_view("editor-md-gutter", "# Title\nbody");
        paint_plain(&mut h, &mut v);
        let text_x = 4.0 * UI.cell_w;
        let nums: Vec<&crate::plugin::TextItem> = h.frame.text.iter().filter(|t| t.left < text_x).collect();
        assert_eq!(nums.len(), 2);
        let heading = h.frame.text.iter().find(|t| t.left == text_x).unwrap();
        let baseline = |src| match src {
            TextSrc::Row(key) => h.text.row_cache.buffer(key).unwrap().layout_runs().next().unwrap().line_y,
            TextSrc::Scratch(_) => panic!("a cached row"),
        };
        let (b_num, b_head) = (baseline(nums[0].src), baseline(heading.src));
        assert_eq!(nums[0].scale, 1.0);
        // The number's baseline meets the heading's, drawn H1 times as far down.
        let top = nums[0].top;
        assert!((top + b_num - b_head * H1).abs() < 0.5, "top {top}, baselines {b_num} {b_head}");
        assert!((top - b_num * (H1 - 1.0)).abs() < 0.5, "row.y + baseline * (scale - 1): {top}");
        assert_eq!((nums[1].top, nums[1].scale), (R1, 1.0), "a body row's number at its top");
    }

    #[test]
    fn a_click_on_a_heading_row_maps_through_its_scaled_columns() {
        let mut h = Harness::new();
        let mut v = markdown_view("editor-md-click", "# Title here\nbody text");
        paint_plain(&mut h, &mut v);
        let (text_x, cw) = (4.0 * UI.cell_w, UI.cell_w * H1);
        let down = |h: &mut Harness, v: &mut EditorView, pos| {
            h.mouse(v, MouseEvent::Down { pos, clicks: 1, button: MouseButton::Left });
            h.mouse(v, MouseEvent::Up { pos, dragged: false, button: MouseButton::Left });
            v.buffer().cursor
        };
        assert_eq!(down(&mut h, &mut v, (text_x + 4.0 * cw + 1.0, 20.0)), (0, 4));
        // Below the heading the columns are base size again.
        assert_eq!(down(&mut h, &mut v, (text_x + 4.0 * UI.cell_w + 1.0, R1 + 2.0)), (1, 4));
        // A double-click picks the word under the pointer at the heading's size.
        let pos = (text_x + 9.5 * cw, 5.0);
        h.mouse(&mut v, MouseEvent::Down { pos, clicks: 2, button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Up { pos, dragged: false, button: MouseButton::Left });
        assert_eq!(v.buffer().selected_text().as_deref(), Some("here"));
    }

    #[test]
    fn caret_selection_and_diagnostics_on_a_heading_measure_its_cells() {
        let mut h = Harness::new();
        let mut v = markdown_view("editor-md-caret", "# ok bad\nbody");
        v.attach(Client::in_process(Flags, None), "markdown");
        settle(&mut v);
        v.buf.anchor = Some((0, 2));
        v.buf.cursor = (0, 5);
        paint_plain(&mut h, &mut v);
        let (text_x, cw) = (4.0 * UI.cell_w, UI.cell_w * H1);
        let sel = (text_x + 2.0 * cw, 0.0, 3.0 * cw, R1);
        assert!(rects(&h, &h.frame.bg).iter().any(|r| near(*r, sel)), "selection {sel:?}");
        let top = rects(&h, &h.frame.top);
        let caret = (text_x + 5.0 * cw, 0.0, 2.0, R1);
        assert!(top.iter().any(|r| near(*r, caret)), "caret {caret:?} in {top:?}");
        let thick = 1.5;
        let under = (text_x + 5.0 * cw, R1 - thick, 3.0 * cw, thick);
        assert!(top.iter().any(|r| near(*r, under)), "underline {under:?} in {top:?}");
    }

    #[test]
    fn a_partial_heading_row_at_the_bottom_draws_nothing_below_the_body() {
        let mut h = Harness::new();
        // Eleven body rows (176px), then an H1 row running 5px past the 200px body.
        let text = format!("{}# ok bad", "x\n".repeat(11));
        let mut v = markdown_view("editor-md-clip", &text);
        v.attach(Client::in_process(Flags, None), "markdown");
        settle(&mut v);
        v.buf.anchor = Some((11, 2));
        v.buf.cursor = (11, 5);
        paint_plain(&mut h, &mut v);
        // Following the cursor scrolled it whole; put the row back half shown.
        v.buf.scroll = 0;
        paint_plain(&mut h, &mut v);
        let all: Vec<_> = rects(&h, &h.frame.bg).into_iter().chain(rects(&h, &h.frame.top)).collect();
        assert!(all.iter().all(|r| r.1 + r.3 <= RECT.h + 0.01), "past the body: {all:?}");
        let (text_x, cw, y) = (4.0 * UI.cell_w, UI.cell_w * H1, 11.0 * UI.cell_h);
        let caret = (text_x + 5.0 * cw, y, 2.0, RECT.h - y);
        assert!(all.iter().any(|r| near(*r, caret)), "the caret, cut: {caret:?}");
        let sel = (text_x + 2.0 * cw, y, 3.0 * cw, RECT.h - y);
        assert!(all.iter().any(|r| near(*r, sel)), "the selection, cut: {sel:?}");
    }

    #[test]
    fn following_the_cursor_fits_tall_rows() {
        let mut h = Harness::new();
        let text: String = (0..20).map(|i| format!("# H{i}\n")).collect();
        let mut v = markdown_view("editor-md-follow", &text);
        paint_plain(&mut h, &mut v);
        v.buf.cursor = (10, 0);
        paint_plain(&mut h, &mut v);
        // Six H1 rows (174px) fit the 200px body, seven don't.
        assert_eq!(v.buffer().scroll, 5);
        let caret = rects(&h, &h.frame.top).into_iter().find(|r| r.2 == 2.0).expect("a caret");
        assert_eq!((caret.1, caret.3), (5.0 * R1, R1), "whole, on the last whole row");
    }

    #[test]
    fn a_long_heading_wraps_at_its_scaled_column_count() {
        let mut h = Harness::new();
        // 62 chars: 2 rows at 46 base columns, 3 at H1's 25 (breaking after a space).
        let line = format!("# {}", "word ".repeat(12));
        let mut v = markdown_view("editor-md-wrap", &format!("{line}\nx"));
        v.soft_wrap = Some(true);
        paint_plain(&mut h, &mut v);
        assert_eq!(v.layout.rows.cols(H1), 25);
        let rows: Vec<(usize, usize, usize)> = v.rows.text_rows().map(|r| (r.ln, r.start, r.len)).collect();
        assert_eq!(rows, [(0, 0, 22), (0, 22, 25), (0, 47, 15), (1, 0, 1)]);
        assert_eq!(text_rows(&h), vec![(0.0, H1), (R1, H1), (2.0 * R1, H1), (3.0 * R1, 1.0)]);
        // A click at the second row's left edge lands on its first column.
        let pos = (4.0 * UI.cell_w + 1.0, R1 + 2.0);
        h.mouse(&mut v, MouseEvent::Down { pos, clicks: 1, button: MouseButton::Left });
        assert_eq!(v.buffer().cursor, (0, 22));
    }

    #[test]
    fn a_scaled_row_scrolled_sideways_measures_scaled_columns() {
        let mut h = Harness::new();
        let mut v = markdown_view("editor-md-hscroll", "# heading long line\nplain");
        v.soft_wrap = Some(false);
        v.buf.hscroll = 2;
        v.buf.anchor = Some((0, 4));
        v.buf.cursor = (0, 6);
        paint_plain(&mut h, &mut v);
        assert_eq!(v.buffer().hscroll, 2, "the caret was in view");
        let (text_x, cw) = (4.0 * UI.cell_w, UI.cell_w * H1);
        let heading = h.frame.text.iter().find(|t| t.scale == H1).unwrap();
        assert!((heading.left - (text_x - 2.0 * cw)).abs() < 0.01, "shifted by two scaled columns");
        let caret = (text_x + 4.0 * cw, 0.0, 2.0, R1);
        assert!(rects(&h, &h.frame.top).iter().any(|r| near(*r, caret)), "caret {caret:?}");
        let sel = (text_x + 2.0 * cw, 0.0, 2.0 * cw, R1);
        assert!(rects(&h, &h.frame.bg).iter().any(|r| near(*r, sel)), "selection {sel:?}");
        let pos = (text_x + 3.0 * cw + 1.0, 5.0);
        h.mouse(&mut v, MouseEvent::Down { pos, clicks: 1, button: MouseButton::Left });
        assert_eq!(v.buffer().cursor, (0, 5));
    }

    #[test]
    fn the_wheel_passes_a_heading_in_its_own_height() {
        let h = Harness::new();
        let mut hh = Harness::new();
        let text: String = (0..20).map(|i| format!("# H{i}\n")).collect();
        let mut v = markdown_view("editor-md-wheel", &text);
        paint_plain(&mut hh, &mut v);
        h.scroll(&mut v, (10.0, 10.0), -UI.cell_h);
        assert_eq!(v.buffer().scroll, 0, "a base row's travel doesn't pass an H1 row");
        h.scroll(&mut v, (10.0, 10.0), -(R1 - UI.cell_h));
        assert_eq!(v.buffer().scroll, 1, "an H1 row's does");
        h.scroll(&mut v, (10.0, 10.0), R1);
        assert_eq!(v.buffer().scroll, 0, "and back");
        // Base rows move a line per cell.
        let mut plain = markdown_view("editor-md-wheel-plain", &"x\n".repeat(20));
        paint_plain(&mut hh, &mut plain);
        h.scroll(&mut plain, (10.0, 10.0), -2.0 * UI.cell_h);
        assert_eq!(plain.buffer().scroll, 2);
    }

    #[test]
    fn code_boxes_sit_on_their_rows_below_a_heading() {
        let mut h = Harness::new();
        let mut v = markdown_view("editor-md-boxes", "# Head\ntext `x`\n```\ncode\n```");
        paint_plain(&mut h, &mut v);
        let (ch, line) = (UI.cell_h, theme::hairline(UI));
        let fence = R1 + ch;
        let boxes = code_boxes(&h);
        assert_eq!(boxes.len(), 2, "{boxes:?}");
        // The inline box on the prose row, the block over the three fence rows.
        let inline = boxes.iter().find(|b| b.3 < ch).expect("inline box");
        assert_eq!((inline.1, inline.3), (R1 + line, ch - 2.0 * line));
        let block = boxes.iter().find(|b| b.3 > ch).expect("block box");
        assert_eq!((block.1, block.3), (fence + line, 3.0 * ch - 2.0 * line));
    }

    #[test]
    fn toml_tables_draw_by_their_level() {
        let mut h = Harness::new();
        let mut v = file_view("editor-toml-sized", "doc.toml", "[a]\nk = 1\n[a.b]\n[[bin]]\n[a.b.c]\n  [1, 2],");
        paint_plain(&mut h, &mut v);
        let scales: Vec<f32> = text_rows(&h).into_iter().map(|r| r.1).collect();
        assert_eq!(scales, vec![H1, 1.0, H2, H2, 1.25, 1.0]);
    }

    #[test]
    fn hashes_in_a_fence_quote_or_list_are_not_headings() {
        let mut h = Harness::new();
        let mut v = markdown_view("editor-md-nested", "```\n# code\n```\n> # quoted\n- # item\n# real");
        paint_plain(&mut h, &mut v);
        let scales: Vec<f32> = text_rows(&h).into_iter().map(|r| r.1).collect();
        assert_eq!(scales, vec![1.0, 1.0, 1.0, 1.0, 1.0, H1]);
    }
}
