//! The Markdown Preview: a document's text (unsaved edits included) rendered
//! read-only. [`parse`] turns the text into blocks; this lays them out in one
//! centred column, top to bottom, and draws each through the block cache
//! (`PaintCx::block`): serif body text, sized bold headings, inline code on
//! pills, underlined links, bordered code blocks and tables, quote bars,
//! list markers and task boxes.
//!
//! The column scrolls in pixels from an anchor, a block and how far into it
//! the top of the view is, so blocks above the anchor that get measured
//! (their estimate replaced by their shaped height) never move what shows.
//! Each block's height is kept here, not read back from the cache, which may
//! evict an idle block.

pub mod parse;

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::Instant;

use ghostrealm_core::highlight;
use ghostrealm_core::Rect;
use ghostrealm_terminal::{Key, KeyPress};
use winit::window::CursorIcon;

use crate::block_cache::{block_key, estimate_height, BlockSpan};
use crate::plugin::icons::Icon;
use crate::plugin::paint::intersect;
use crate::plugin::{theme, Font, PaintCx, Request, Waker};
use crate::syntax;
use parse::{Block, Inline, Kind, Marker};

use super::{paint_copy_button, EDITOR_BG, EDITOR_FG};

/// A text this long (bytes) or longer parses on a worker; shorter parses
/// at once, under a millisecond in a release build.
const PARSE_INLINE_MAX: usize = 64 * 1024;
/// Body text size, times the editor's: a serif reads smaller than the
/// monospace beside it at one size.
const BODY_SCALE: f32 = 1.15;
/// Line height, times the text size.
const LEADING: f32 = 1.5;
/// The reading column's widest, in body text sizes.
const COLUMN_MAX: f32 = 46.0;

/// The first button id of the links drawn. A link's id is this plus its
/// block's index shifted past [`TAG_BITS`], plus its tag: tied to the link,
/// not to where it painted.
pub const BTN_LINK: u32 = 1 << 30;
/// The bits of a link's id that hold its tag: a block's links past
/// `1 << TAG_BITS` tags, or a block past `1 << 20`, get no button.
const TAG_BITS: u32 = 10;
const MAX_LINK_BLOCK: usize = 1 << 20;

/// What the per-block layouts were made for: a change starts them over.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Look {
    /// Body text size and line height (px).
    size: f32,
    line_h: f32,
    /// The editor's monospace size and cell (code).
    mono: f32,
    cell: (f32, f32),
    /// The reading column's width.
    width: f32,
    accent: [u8; 3],
    ui_scale: f32,
}

/// What a tagged run of a text block is, for the quads drawn over it.
#[derive(Clone, Debug, PartialEq)]
enum Tag {
    Code,
    Link(String),
    Strike,
}

/// A block of inline text ready to draw: its spans, their content key, and
/// what each tag marks.
#[derive(Clone, Debug)]
struct Text {
    key: u64,
    spans: Vec<BlockSpan>,
    tags: Vec<(NonZeroU32, Tag)>,
}

#[derive(Clone, Debug)]
enum Prep {
    Text { text: Text, marker: Option<MarkerPrep> },
    Code { rows: Vec<Vec<(String, [u8; 3])>> },
    /// The header row first. `cols`: the column widths at the look's width,
    /// once measured.
    Table { cells: Vec<Vec<Text>>, cols: Option<Vec<f32>> },
    Rule,
}

#[derive(Clone, Debug)]
enum MarkerPrep {
    Text(Text),
    Task(bool),
}

/// A block's layout state.
#[derive(Clone, Debug, Default)]
struct Laid {
    prep: Option<Prep>,
    /// Its slot's height (the gap above it included), and whether that was
    /// measured rather than estimated.
    height: Option<(f32, bool)>,
}

/// Where the view's top is: `offset` px into block `index`'s slot.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Anchor {
    index: usize,
    offset: f32,
}

/// What a press in the Preview asks of the editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Edit the source at this line (a double click).
    EditAt(usize),
}

/// What the Preview draws from: the editor's buffer.
pub struct Source<'a> {
    pub lines: &'a [String],
    pub edits: u64,
    /// The code block (by its first line) whose copy button shows a check.
    pub copied: Option<usize>,
}

pub struct Preview {
    waker: Option<Waker>,
    blocks: Vec<Block>,
    /// The edit count `blocks` was parsed from (`None`: nothing parsed).
    parsed: Option<u64>,
    /// A parse on the worker: the edit count it is of, and its result.
    job: Option<(u64, Receiver<Vec<Block>>)>,
    look: Option<Look>,
    laid: Vec<Laid>,
    anchor: Anchor,
    /// A source line to bring to the top once the blocks are in.
    goto_line: Option<usize>,
    /// After a resize, the share of the anchor block above the top, until
    /// the block is measured at the new width.
    anchor_frac: Option<f32>,
    /// The last paint's body, and blocks on screen (index, slot top,
    /// bottom).
    body: Rect,
    shown: Vec<(usize, f32, f32)>,
}

impl Preview {
    pub fn new(waker: Option<Waker>) -> Self {
        Preview {
            waker,
            blocks: Vec::new(),
            parsed: None,
            job: None,
            look: None,
            laid: Vec::new(),
            anchor: Anchor::default(),
            goto_line: None,
            anchor_frac: None,
            body: Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
            shown: Vec::new(),
        }
    }

    /// Parse the text if it changed since last parsed: at once if short,
    /// else on a worker, showing the blocks as they were until it is done.
    /// One parse runs at a time: edits made meanwhile parse once it lands.
    /// The lines are joined here: cloning them for the worker to join costs
    /// more than the join (an allocation per line against one copy).
    fn sync(&mut self, src: &Source) {
        self.poll();
        if self.parsed == Some(src.edits) || self.job.is_some() {
            return;
        }
        let len: usize = src.lines.iter().map(|l| l.len() + 1).sum();
        let text = src.lines.join("\n");
        if len < PARSE_INLINE_MAX {
            self.job = None;
            self.adopt(parse::parse(&text), src.edits);
            return;
        }
        let (tx, rx) = mpsc::channel();
        let waker = self.waker.clone();
        std::thread::spawn(move || {
            let _ = tx.send(parse::parse(&text));
            if let Some(w) = waker {
                w();
            }
        });
        self.job = Some((src.edits, rx));
    }

    /// Take a finished parse. Returns whether one came in.
    pub fn poll(&mut self) -> bool {
        let Some((edits, rx)) = &self.job else { return false };
        let edits = *edits;
        match rx.try_recv() {
            Ok(blocks) => {
                self.job = None;
                self.adopt(blocks, edits);
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.job = None;
                false
            }
        }
    }

    /// Show `blocks`, keeping the source line at the top where it was.
    fn adopt(&mut self, blocks: Vec<Block>, edits: u64) {
        if self.goto_line.is_none() && self.parsed.is_some() {
            self.goto_line = self.top_line();
        }
        self.laid = vec![Laid::default(); blocks.len()];
        self.blocks = blocks;
        self.parsed = Some(edits);
        // They index the old blocks.
        self.shown.clear();
        self.anchor_frac = None;
    }

    /// Whether the blocks shown are of edit count `edits`.
    pub fn is_current(&self, edits: u64) -> bool {
        self.parsed == Some(edits)
    }

    /// The source line at the top of the view: the anchor block's, or the
    /// next block's once more than half of it has scrolled past.
    pub fn top_line(&self) -> Option<usize> {
        // A line asked for but not yet shown is where the top will be.
        if self.goto_line.is_some() {
            return self.goto_line;
        }
        let Anchor { index, offset } = self.anchor;
        let block = self.blocks.get(index)?;
        let past_half = self.laid[index].height.is_some_and(|(h, _)| offset > h * 0.5);
        match self.blocks.get(index + 1) {
            Some(next) if past_half => Some(next.line),
            _ => Some(block.line),
        }
    }

    /// Bring the block holding source line `line` to the top at the next
    /// paint.
    pub fn show_line(&mut self, line: usize) {
        self.goto_line = Some(line);
    }

    /// The block at height `y` in the last paint.
    fn block_at(&self, y: f32) -> Option<usize> {
        self.shown.iter().find(|(_, top, bottom)| y >= *top && y < *bottom).map(|s| s.0)
    }

    /// A press: a double click edits the source at the block under it.
    pub fn press(&self, (x, y): (f32, f32), clicks: u32) -> Option<Action> {
        if clicks != 2 || !crate::plugin::rect_contains(self.body, x, y) {
            return None;
        }
        let i = self.block_at(y)?;
        Some(Action::EditAt(self.blocks.get(i)?.line))
    }

    /// The target of the link with button id `BTN_LINK + i`.
    fn link(&mut self, i: u32) -> Option<String> {
        let (block, tag) = ((i >> TAG_BITS) as usize, i & ((1 << TAG_BITS) - 1));
        if block >= self.blocks.len() || self.look.is_none() {
            return None;
        }
        let texts: Vec<&Text> = match self.prep(block) {
            Prep::Text { text, .. } => vec![&*text],
            Prep::Table { cells, .. } => cells.iter().flatten().collect(),
            _ => Vec::new(),
        };
        texts.iter().flat_map(|t| &t.tags).find_map(|(t, kind)| match kind {
            Tag::Link(url) if t.get() == tag => Some(url.clone()),
            _ => None,
        })
    }

    /// The body of the code block starting on source line `line`.
    pub fn code_text(&self, line: usize) -> Option<String> {
        self.blocks.iter().find_map(|b| match &b.kind {
            Kind::Code { text, .. } | Kind::FrontMatter(text) if b.line == line && !text.trim().is_empty() => {
                Some(text.clone())
            }
            _ => None,
        })
    }

    /// Scroll by `dy` px (positive: further down the document). Returns
    /// whether the view moved.
    pub fn scroll_by(&mut self, dy: f32) -> bool {
        if self.look.is_none() || self.blocks.is_empty() {
            return false;
        }
        let before = self.anchor;
        self.anchor.offset += dy;
        self.normalize();
        self.clamp_bottom();
        self.anchor != before
    }

    /// Scrolling keys: arrows, page keys, Space, Home/End (also Cmd+Up and
    /// Cmd+Down). Returns whether the key scrolls.
    pub fn key(&mut self, k: &KeyPress) -> Option<bool> {
        let look = self.look?;
        let page = (self.body.h - look.line_h).max(look.line_h);
        let m = k.mods;
        let dy = match k.key {
            Key::Up if m.super_ => f32::NEG_INFINITY,
            Key::Down if m.super_ => f32::INFINITY,
            Key::Up => -2.0 * look.line_h,
            Key::Down => 2.0 * look.line_h,
            Key::PageUp => -page,
            Key::PageDown => page,
            Key::Char(' ') if m.shift => -page,
            Key::Char(' ') => page,
            Key::Home => f32::NEG_INFINITY,
            Key::End => f32::INFINITY,
            _ => return None,
        };
        if m.ctrl || m.alt || (m.super_ && !matches!(k.key, Key::Up | Key::Down)) {
            return None;
        }
        let moved = if dy == f32::NEG_INFINITY {
            let before = self.anchor;
            self.anchor = Anchor::default();
            before != self.anchor
        } else if dy == f32::INFINITY {
            let before = self.anchor;
            self.anchor = Anchor { index: self.blocks.len().saturating_sub(1), offset: 0.0 };
            self.clamp_bottom();
            before != self.anchor
        } else {
            self.scroll_by(dy)
        };
        Some(moved)
    }

    /// Block `i`'s slot height: as measured, else estimated.
    fn slot_h(&mut self, i: usize) -> f32 {
        if let Some((h, _)) = self.laid[i].height {
            return h;
        }
        let Some(look) = self.look else { return 0.0 };
        let h = gap(&self.blocks, i, &look) + self.estimate(i, &look);
        self.laid[i].height = Some((h, false));
        h
    }

    /// Roughly how tall block `i`'s content is, unshaped.
    fn estimate(&mut self, i: usize, look: &Look) -> f32 {
        let w = text_width(&self.blocks[i], look);
        match self.prep(i) {
            Prep::Text { text, .. } => estimate_height(&text.spans, w),
            Prep::Code { rows } => code_height(rows.len(), look),
            Prep::Table { cells, .. } => cells.len() as f32 * (look.line_h + 2.0 * cell_pad(look).1),
            Prep::Rule => rule_height(look),
        }
    }

    /// Block `i`'s spans and rows, built once per look.
    fn prep(&mut self, i: usize) -> &mut Prep {
        let look = self.look.expect("a look before laying out");
        let (blocks, laid) = (&self.blocks, &mut self.laid);
        laid[i].prep.get_or_insert_with(|| prepare(&blocks[i], &look))
    }

    /// Keep the anchor's offset inside its block's slot.
    fn normalize(&mut self) {
        let last = self.blocks.len().saturating_sub(1);
        self.anchor.index = self.anchor.index.min(last);
        while self.anchor.offset < 0.0 && self.anchor.index > 0 {
            self.anchor.index -= 1;
            self.anchor.offset += self.slot_h(self.anchor.index);
        }
        self.anchor.offset = self.anchor.offset.max(0.0);
        while self.anchor.index < last {
            let h = self.slot_h(self.anchor.index);
            if self.anchor.offset < h {
                break;
            }
            self.anchor.offset -= h;
            self.anchor.index += 1;
        }
    }

    /// Scroll back up so the document's end, padded, isn't above the
    /// body's bottom (unless the whole document fits).
    fn clamp_bottom(&mut self) {
        let Some(look) = self.look else { return };
        let want = self.body.h - look.size;
        let mut below = -self.anchor.offset;
        let mut i = self.anchor.index;
        while i < self.blocks.len() && below < want {
            below += self.slot_h(i);
            i += 1;
        }
        if below < want {
            self.anchor.offset -= want - below;
            self.normalize();
        }
    }

    /// Draw the document into `body`.
    pub fn paint(&mut self, cx: &mut PaintCx, body: Rect, src: &Source, doc: Option<&Path>) {
        cx.fill(body, EDITOR_BG);
        self.body = body;
        self.shown.clear();
        self.sync(src);
        if self.parsed.is_none() {
            let ch = cx.ui.cell_h;
            cx.label("Rendering…", Font::SANS, body.x + ch, body.y + ch, body, theme::TEXT_DIM);
            return;
        }
        let look = look_of(cx, body);
        if self.look != Some(look) {
            // The anchor block keeps the same share of it above the top
            // as it reflows (exact once it is measured again).
            let index = self.anchor.index;
            let frac = self.laid.get(index).and_then(|l| l.height).map(|(h, _)| self.anchor.offset / h.max(1.0));
            self.look = Some(look);
            self.laid.iter_mut().for_each(|l| *l = Laid::default());
            if let (Some(f), true) = (frac, index < self.blocks.len()) {
                self.anchor.offset = f * self.slot_h(index);
                self.anchor_frac = Some(f);
            }
        }
        if let Some(line) = self.goto_line.take() {
            let index = self.blocks.partition_point(|b| b.line <= line).saturating_sub(1);
            self.anchor = Anchor { index, offset: 0.0 };
        }
        if self.blocks.is_empty() {
            return;
        }
        self.normalize();
        self.clamp_bottom();

        let left = body.x + ((body.w - look.width) * 0.5).round();
        let mut y = body.y - self.anchor.offset;
        let mut i = self.anchor.index;
        let mut grew = false;
        while i < self.blocks.len() && y < body.y + body.h {
            let g = gap(&self.blocks, i, &look);
            let top = y + g;
            let content = self.paint_block(cx, i, left, top, &look, src, doc);
            let measured = content.is_some();
            let h = g + content.unwrap_or_else(|| self.estimate(i, &look));
            let old = self.laid[i].height;
            if old.is_some_and(|(old, _)| (old - h).abs() > 0.5) {
                grew = true;
            }
            self.laid[i].height = Some((h, measured));
            if i == self.anchor.index && measured {
                self.settle_anchor(old, h, cx);
            }
            self.paint_quotes(cx, i, left, (y, top, y + h), &look);
            self.shown.push((i, y, y + h));
            y += h;
            i += 1;
        }
        // Measuring may leave the end short of the bottom: settle next frame.
        if grew && i == self.blocks.len() {
            cx.request_frame();
        }
    }

    /// Draw block `i` with its content's top-left at `(left + indent, top)`.
    /// Returns its content's height, `None` while unshaped.
    #[allow(clippy::too_many_arguments)]
    fn paint_block(
        &mut self,
        cx: &mut PaintCx,
        i: usize,
        left: f32,
        top: f32,
        look: &Look,
        src: &Source,
        doc: Option<&Path>,
    ) -> Option<f32> {
        let block = &self.blocks[i];
        let x = left + indent_x(block, look);
        let w = text_width(block, look);
        let (line, lang) = (block.line, match &block.kind {
            Kind::Code { lang, .. } => lang.clone(),
            _ => None,
        });
        let body = self.body;
        let links = Links { block: i, doc };
        // Out while it draws, back after.
        let mut prep = self.laid[i].prep.take().unwrap_or_else(|| prepare(block, look));
        let out = match &mut prep {
            Prep::Text { text, marker } => {
                if let Some(m) = marker {
                    paint_marker(cx, m, x - list_indent(look), top, body, look);
                }
                paint_text(cx, text, (x, top, w), body, look, links)
            }
            Prep::Code { rows } => {
                let copied = src.copied == Some(line);
                Some(paint_code(cx, rows, lang.as_deref(), (x, top, w), body, line, copied, look))
            }
            Prep::Table { cells, cols } => {
                let cols = cols.get_or_insert_with(|| column_widths(cx, cells, w, look));
                paint_table(cx, cells, cols, (x, top), body, look, links)
            }
            Prep::Rule => {
                let line = theme::hairline(cx.ui);
                let r = Rect { x, y: (top + rule_height(look) * 0.5).round(), w, h: line };
                cx.with_clip(body, |cx| cx.fill_alpha(r, theme::GUIDE, theme::GUIDE_ALPHA * 2.0));
                Some(rule_height(look))
            }
        };
        self.laid[i].prep = Some(prep);
        out
    }

    /// The anchor block was just measured at `h` (it was `old`): keep what
    /// shows at the top where it was. Scrolled into from below, its offset
    /// counted from its estimated height; after a resize, it keeps its
    /// share. Repaints next frame with the new offset.
    fn settle_anchor(&mut self, old: Option<(f32, bool)>, h: f32, cx: &mut PaintCx) {
        let before = self.anchor.offset;
        if let Some(f) = self.anchor_frac.take() {
            self.anchor.offset = f * h;
        } else if let Some((old, false)) = old {
            if before > 0.0 {
                self.anchor.offset = (before + h - old).clamp(0.0, (h - 1.0).max(0.0));
            }
        }
        if (self.anchor.offset - before).abs() > 0.5 {
            cx.request_frame();
        }
    }

    /// Block `i`'s quote bars: one per quote it is in, joined to the bars
    /// of the block above where it is quoted as deep.
    fn paint_quotes(&self, cx: &mut PaintCx, i: usize, left: f32, (slot_top, top, bottom): (f32, f32, f32), look: &Look) {
        let depth = self.blocks[i].quote;
        let above = i.checked_sub(1).map_or(0, |p| self.blocks[p].quote);
        let w = (3.0 * look.ui_scale).round();
        for q in 0..depth {
            let from = if q < above { slot_top } else { top };
            let x = left + q as f32 * quote_indent(look);
            let r = Rect { x, y: from.round(), w, h: (bottom - from).round() };
            cx.with_clip(self.body, |cx| cx.fill_alpha(r, theme::GUIDE, theme::GUIDE_ALPHA * 2.5));
        }
    }

    /// What the link with button id `BTN_LINK + i` asks, resolved against
    /// the document at `doc` ([`target`]): a folder shows in the sidebar; a
    /// path that is missing or not a regular file (a device, a FIFO) opens
    /// nothing.
    pub fn open_link(&mut self, i: u32, doc: Option<&Path>) -> Option<Request> {
        match target(&self.link(i)?, doc)? {
            Target::Web(url) => Some(Request::OpenUrl(url)),
            Target::File(path) => match std::fs::metadata(&path) {
                Ok(m) if m.is_file() => Some(Request::OpenFile(path)),
                Ok(m) if m.is_dir() => Some(Request::ShowInSidebar(path)),
                _ => None,
            },
            Target::Blocked(_) | Target::InPage => None,
        }
    }
}

/// Where a link goes.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// An `http`, `https`, or `mailto` URL: the system's handler opens it.
    Web(String),
    /// A local path (relative to the document's folder, or a `file:` URL):
    /// it opens in the app, never in another program.
    File(PathBuf),
    /// Any other scheme: not opened, as it could run something.
    Blocked(String),
    /// `#anchor`: within the document (not followed yet).
    InPage,
}

/// Where link `url` in the document at `doc` goes; `None` for a relative
/// path with no document folder to resolve it against.
pub fn target(url: &str, doc: Option<&Path>) -> Option<Target> {
    if url.starts_with('#') || url.is_empty() {
        return Some(Target::InPage);
    }
    if let Some(scheme) = crate::file_ops::url_scheme(url) {
        if crate::file_ops::is_web_url(url) {
            return Some(Target::Web(url.to_string()));
        }
        if !scheme.eq_ignore_ascii_case("file") {
            return Some(Target::Blocked(scheme.to_ascii_lowercase()));
        }
        // `file:///p`, `file://localhost/p`, `file:/p`.
        let rest = &url[scheme.len() + 1..];
        let path = match rest.strip_prefix("//") {
            Some(r) => r.strip_prefix("localhost").unwrap_or(r),
            None => rest,
        };
        let path = PathBuf::from(percent_decode(path.split(['#', '?']).next().unwrap_or(path)));
        return match path.is_absolute() {
            true => Some(Target::File(path)),
            false => Some(Target::Blocked("file".into())),
        };
    }
    let path = url.split(['#', '?']).next().unwrap_or(url);
    let path = PathBuf::from(percent_decode(path));
    let path = match path.is_absolute() {
        true => path,
        false => doc?.parent()?.join(path),
    };
    Some(Target::File(path))
}

/// What a link's hover says: where it goes, or why it doesn't.
fn link_tip(url: &str, doc: Option<&Path>) -> String {
    match target(url, doc) {
        Some(Target::Web(u)) => u,
        Some(Target::File(p)) => p.display().to_string(),
        Some(Target::Blocked(s)) => format!("Not opened: {s}: links"),
        Some(Target::InPage) => format!("{url} (links within the document don't follow yet)"),
        None => format!("{url} (no folder to find it from: save the document first)"),
    }
}

/// Which links a text block's tags are: its block (for their button ids)
/// and its document (for their hover).
#[derive(Clone, Copy)]
struct Links<'a> {
    block: usize,
    doc: Option<&'a Path>,
}

/// `%XX` escapes decoded (a link to a file whose name has spaces); a
/// malformed escape stays as written.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn look_of(cx: &PaintCx, body: Rect) -> Look {
    let mono = cx.metrics().font_size;
    let size = (mono * BODY_SCALE).round();
    let pad = 2.0 * cx.ui.cell_w;
    let width = (body.w - 2.0 * pad).min(COLUMN_MAX * size).max(1.0).round();
    Look {
        size,
        line_h: (size * LEADING).round(),
        mono,
        cell: (cx.ui.cell_w, cx.ui.cell_h),
        width,
        accent: cx.chrome.accent,
        ui_scale: cx.ui.scale,
    }
}

/// How far a list level indents (a marker's room).
fn list_indent(look: &Look) -> f32 {
    (look.size * 1.6).round()
}

/// How far a quote level indents (its bar and the room after it).
fn quote_indent(look: &Look) -> f32 {
    (look.size * 1.1).round()
}

fn indent_x(block: &Block, look: &Look) -> f32 {
    block.quote as f32 * quote_indent(look) + block.indent as f32 * list_indent(look)
}

/// The width block's content gets: the column, less its indent.
fn text_width(block: &Block, look: &Look) -> f32 {
    (look.width - indent_x(block, look)).max(look.size * 4.0)
}

/// The space above block `i`: more above a heading, less between a list's
/// items.
fn gap(blocks: &[Block], i: usize, look: &Look) -> f32 {
    let s = look.size;
    let Some(prev) = i.checked_sub(1).map(|p| &blocks[p]) else { return (s * 0.5).round() };
    let b = &blocks[i];
    let em = match b.kind {
        Kind::Heading(l, _) if l <= 2 => 1.5,
        Kind::Heading(..) => 1.1,
        Kind::Item(..) if matches!(prev.kind, Kind::Item(..)) || prev.indent > 0 => 0.3,
        _ if b.indent > 0 && prev.indent > 0 => 0.5,
        _ => 0.8,
    };
    (s * em).round()
}

/// A code block's padding: (sides, top and bottom).
fn code_pad(look: &Look) -> (f32, f32) {
    (look.cell.0.round(), (look.cell.1 * 0.6).round())
}

fn code_height(rows: usize, look: &Look) -> f32 {
    rows.max(1) as f32 * look.cell.1 + 2.0 * code_pad(look).1
}

fn rule_height(look: &Look) -> f32 {
    look.size.round()
}

/// A table cell's padding: (sides, top and bottom).
fn cell_pad(look: &Look) -> (f32, f32) {
    ((look.size * 0.8).round(), (look.size * 0.45).round())
}

/// Spans for a block's inline runs in `font` at `size` (line height
/// `line_h`), in `color`: inline code monospace at the editor's size, links
/// in the accent, each code span and link tagged from `next` up (a table's
/// cells share one count, so a tag names one link in the block).
fn inline_text(runs: &[Inline], font: Font, size: f32, line_h: f32, color: [u8; 3], look: &Look, next: &mut u32) -> Text {
    let mut spans = Vec::with_capacity(runs.len());
    let mut tags: Vec<(NonZeroU32, Tag)> = Vec::new();
    let mut new_tag = |tags: &mut Vec<(NonZeroU32, Tag)>, tag: Tag| {
        let t = NonZeroU32::new(*next).expect("tags count from 1");
        *next += 1;
        tags.push((t, tag));
        t
    };
    // Consecutive runs of one link share its tag (bold within a link).
    let mut link_tag: Option<(String, NonZeroU32)> = None;
    for r in runs {
        let face = font.bold(font.bold || r.style.bold).italic(r.style.italic || r.style.image);
        // Code keeps the editor's size beside body text, scaled as a
        // heading's text is.
        let code_size = (size * look.mono / look.size).round();
        let (face, run_size, mut c) = match r.style.code {
            true => (Font::MONO.bold(face.bold), code_size, syntax::color(highlight::Token::Code, EDITOR_FG)),
            false => (face, size, color),
        };
        if r.style.image {
            c = theme::TEXT_DIM;
        }
        let text = if r.style.image { format!("[{}]", r.text) } else { r.text.clone() };
        let mut span = BlockSpan::new(text, face, run_size, line_h, c);
        if let Some(url) = &r.link {
            let tag = match &link_tag {
                Some((u, t)) if u == url => *t,
                _ => {
                    let t = new_tag(&mut tags, Tag::Link(url.clone()));
                    link_tag = Some((url.clone(), t));
                    t
                }
            };
            span = BlockSpan { color: look.accent, ..span }.tagged(tag);
        } else {
            link_tag = None;
            if r.style.code {
                span = span.tagged(new_tag(&mut tags, Tag::Code));
            } else if r.style.strike {
                span = span.tagged(new_tag(&mut tags, Tag::Strike));
            }
        }
        spans.push(span);
    }
    if spans.is_empty() {
        spans.push(BlockSpan::new("", font, size, line_h, color));
    }
    Text { key: block_key(&spans), spans, tags }
}

/// Block `block`'s spans and rows.
fn prepare(block: &Block, look: &Look) -> Prep {
    let color = if block.quote > 0 { theme::TEXT_DIM } else { EDITOR_FG };
    let next = &mut 1;
    let mut body = |runs: &[Inline], bold: bool| inline_text(runs, Font::SERIF.bold(bold), look.size, look.line_h, color, look, next);
    match &block.kind {
        Kind::Heading(level, runs) => {
            let size = (look.size * syntax::heading_scale(Some(*level))).round();
            let text = inline_text(runs, Font::SERIF.bold(true), size, (size * 1.3).round(), color, look, &mut 1);
            Prep::Text { text, marker: None }
        }
        Kind::Paragraph(runs) => Prep::Text { text: body(runs, false), marker: None },
        Kind::Item(marker, runs) => {
            let marker = match *marker {
                Marker::Task(done) => MarkerPrep::Task(done),
                Marker::Bullet => {
                    let bullet = if block.indent % 2 == 1 { "•" } else { "◦" };
                    MarkerPrep::Text(plain_text(bullet, Font::SERIF, look, theme::TEXT_DIM))
                }
                Marker::Number(n) => MarkerPrep::Text(plain_text(&format!("{n}."), Font::SERIF, look, theme::TEXT_DIM)),
            };
            Prep::Text { text: body(runs, false), marker: Some(marker) }
        }
        Kind::Html(text) => {
            let span = BlockSpan::new(text.clone(), Font::MONO, look.mono, look.cell.1, theme::TEXT_DIM);
            Prep::Text { text: Text { key: block_key(std::slice::from_ref(&span)), spans: vec![span], tags: Vec::new() }, marker: None }
        }
        Kind::Code { lang, text } => Prep::Code { rows: code_rows(lang.as_deref(), text) },
        // Metadata, not content: boxed as code, dim.
        Kind::FrontMatter(text) => Prep::Code {
            rows: text.split('\n').map(|l| vec![(l.strip_suffix('\r').unwrap_or(l).to_string(), theme::TEXT_DIM)]).collect(),
        },
        Kind::Table { head, rows } => {
            let mut cells = vec![head.iter().map(|c| body(c, true)).collect::<Vec<_>>()];
            cells.extend(rows.iter().map(|r| r.iter().map(|c| body(c, false)).collect()));
            Prep::Table { cells, cols: None }
        }
        Kind::Rule => Prep::Rule,
    }
}

fn plain_text(text: &str, font: Font, look: &Look, color: [u8; 3]) -> Text {
    let span = BlockSpan::new(text, font, look.size, look.line_h, color);
    Text { key: block_key(std::slice::from_ref(&span)), spans: vec![span], tags: Vec::new() }
}

/// A code block's rows, coloured by its language's highlighter if it has
/// one.
fn code_rows(lang: Option<&str>, text: &str) -> Vec<Vec<(String, [u8; 3])>> {
    let lines: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    match lang.and_then(highlight::for_extension) {
        Some(h) => {
            let spans = highlight::highlight_lines(h, &lines);
            lines.iter().zip(&spans).map(|(l, s)| syntax::row_spans(l, s, 0, l.chars().count(), EDITOR_FG)).collect()
        }
        None => lines.iter().map(|l| vec![(l.to_string(), EDITOR_FG)]).collect(),
    }
}

/// Draw `text` `w` wide at `(x, top)`, then its tags' quads: a pill behind
/// inline code, a line under a link (a button: it opens on release) or
/// through struck text. Returns its height, `None` while unshaped.
fn paint_text(
    cx: &mut PaintCx,
    text: &Text,
    (x, top, w): (f32, f32, f32),
    clip: Rect,
    look: &Look,
    links: Links,
) -> Option<f32> {
    let laid = cx.block(text.key, || text.spans.clone(), w, x, top, clip)?;
    let line = theme::hairline(cx.ui);
    let radius = theme::box_radius(cx.ui);
    for (tag, kind) in &text.tags {
        let rects: Vec<Rect> = cx
            .block_rects(text.key, laid.width, *tag)
            .into_iter()
            .map(|r| Rect { x: x + r.x, y: top + r.y, ..r })
            .collect();
        match kind {
            Tag::Code => {
                let h = (look.mono * 1.45).round();
                let pad = (look.mono * 0.3).round();
                for r in rects {
                    let pill = Rect { x: (r.x - pad).round(), y: (r.y + (r.h - h) * 0.5).round(), w: (r.w + 2.0 * pad).round(), h };
                    let border = Some((line, theme::CODE_BOX_BORDER, theme::CODE_BOX_BORDER_ALPHA));
                    cx.with_clip(clip, |cx| cx.round_rect(pill, radius, theme::CODE_BOX_BG, theme::CODE_BOX_ALPHA, border));
                }
            }
            Tag::Link(url) => {
                // A link that goes nowhere gets no button, only its hover.
                let opens = matches!(target(url, links.doc), Some(Target::Web(_) | Target::File(_)));
                let id = (links.block < MAX_LINK_BLOCK && tag.get() < 1 << TAG_BITS && opens)
                    .then(|| BTN_LINK + ((links.block as u32) << TAG_BITS) + tag.get());
                let mut hover = false;
                for r in &rects {
                    if let Some(hit) = intersect(*r, clip) {
                        if let Some(id) = id {
                            hover |= cx.button(hit, id);
                            cx.cursor(hit, CursorIcon::Pointer);
                        }
                        cx.tooltip(hit, link_tip(url, links.doc));
                    }
                }
                let thick = (look.size / 14.0).round().max(1.0);
                for r in rects {
                    if hover {
                        let bg = Rect { x: r.x - 2.0, w: r.w + 4.0, ..r };
                        cx.with_clip(clip, |cx| cx.round_rect(bg, radius, theme::HOVER_BG, theme::HOVER_ALPHA, None));
                    }
                    let under = Rect { y: (r.y + (r.h + look.size) * 0.5).round(), h: thick, ..r };
                    cx.with_clip(clip, |cx| cx.fill_alpha(under, look.accent, if hover { 1.0 } else { 0.6 }));
                }
            }
            Tag::Strike => {
                for r in rects {
                    let through = Rect { y: (r.y + r.h * 0.5).round(), h: line.max(1.0), ..r };
                    cx.with_clip(clip, |cx| cx.fill_alpha(through, EDITOR_FG, 0.8));
                }
            }
        }
    }
    Some(laid.height)
}

/// A list item's marker in the indent left of its text at `(x, top)`.
fn paint_marker(cx: &mut PaintCx, m: &MarkerPrep, x: f32, top: f32, clip: Rect, look: &Look) {
    match m {
        MarkerPrep::Text(t) => {
            cx.block(t.key, || t.spans.clone(), list_indent(look), x, top, clip);
        }
        MarkerPrep::Task(done) => {
            let side = (look.size * 0.85).round();
            let r = Rect { x: x.round(), y: (top + (look.line_h - side) * 0.5).round(), w: side, h: side };
            let radius = theme::box_radius(cx.ui);
            let line = theme::hairline(cx.ui);
            cx.with_clip(clip, |cx| {
                if *done {
                    cx.round_rect(r, radius, look.accent, 1.0, None);
                    let inset = (side * 0.12).round();
                    let icon = Rect { x: r.x + inset, y: r.y + inset, w: side - 2.0 * inset, h: side - 2.0 * inset };
                    Icon::Check.paint(cx, icon, EDITOR_BG);
                } else {
                    cx.round_rect(r, radius, theme::DIM, 0.0, Some((line * 1.5, theme::DIM, 1.0)));
                }
            });
        }
    }
}

/// A code block: the editor's code box `w` wide at `(x, top)`, its rows
/// highlighted and clipped to the box (a long line runs out of view), its
/// language and a copy button (`BTN_COPY_BLOCK + line`) at the top right.
/// Returns its height.
#[allow(clippy::too_many_arguments)]
fn paint_code(
    cx: &mut PaintCx,
    rows: &[Vec<(String, [u8; 3])>],
    lang: Option<&str>,
    (x, top, w): (f32, f32, f32),
    clip: Rect,
    line: usize,
    copied: bool,
    look: &Look,
) -> f32 {
    let h = code_height(rows.len(), look);
    let (px, py) = code_pad(look);
    let hair = theme::hairline(cx.ui);
    let radius = theme::box_radius(cx.ui);
    let rect = Rect { x: x.round(), y: top.round(), w: w.round(), h: h.round() };
    let border = Some((hair, theme::CODE_BOX_BORDER, theme::CODE_BOX_BORDER_ALPHA));
    cx.with_clip(clip, |cx| cx.round_rect(rect, radius, theme::CODE_BOX_BG, theme::CODE_BOX_ALPHA, border));
    let inner = Rect { x: rect.x + px, y: rect.y, w: (rect.w - 2.0 * px).max(1.0), h: rect.h };
    let Some(text_clip) = intersect(inner, clip) else { return h };
    let ch = look.cell.1;
    for (r, row) in rows.iter().enumerate() {
        let y = rect.y + py + r as f32 * ch;
        if y + ch < clip.y || y > clip.y + clip.h {
            continue;
        }
        let len: usize = row.iter().map(|(s, _)| s.chars().count()).sum();
        let shape_w = (len as f32 * look.cell.0).max(inner.w);
        cx.row(row, inner.x, y, shape_w, text_clip, EDITOR_FG);
    }
    let Some(shown) = intersect(rect, clip) else { return h };
    // On the first row's line, as in the editor: the button, and the
    // language left of it.
    let first = rect.y + py;
    let side = look.cell.1;
    let has_body = rows.iter().any(|r| r.iter().any(|(s, _)| !s.trim().is_empty()));
    let id = u32::try_from(line).ok().and_then(|l| super::BTN_COPY_BLOCK.checked_add(l));
    if let (true, Some(id)) = (has_body, id) {
        paint_copy_button(cx, shown, first, id, copied);
    }
    if let Some(lang) = lang {
        let label = cx.shape(lang, Font::SANS, rect.w);
        let right = rect.x + rect.w - hair * 2.0 - side - look.cell.0 * 0.5;
        cx.place(label, (right - label.width).round(), first, shown, theme::TEXT_DIM);
    }
    h
}

/// Column widths for a table `w` wide: each column's widest cell (on one
/// line), stretched to fill `w`, or, when they don't fit, an even share
/// each with what the narrow ones leave going to the wide ones.
fn column_widths(cx: &mut PaintCx, cells: &[Vec<Text>], w: f32, look: &Look) -> Vec<f32> {
    let n = cells.iter().map(Vec::len).max().unwrap_or(0).max(1);
    let pad = 2.0 * cell_pad(look).0;
    // Shaped at the frame's size, then scaled to each span's.
    let frame_size = cx.metrics().font_size.max(1.0);
    let mut natural = vec![look.size * 2.0; n];
    for row in cells {
        for (c, text) in row.iter().enumerate() {
            let mut width = 0.0;
            for s in &text.spans {
                let shaped = cx.shape(&s.text, s.font, f32::MAX / 4.0);
                width += shaped.width * s.size / frame_size;
            }
            natural[c] = natural[c].max(width.ceil() + pad);
        }
    }
    let total: f32 = natural.iter().sum();
    if total <= w {
        return natural.iter().map(|c| (c * w / total).floor()).collect();
    }
    let share = w / n as f32;
    let narrow: f32 = natural.iter().filter(|c| **c <= share).sum();
    let wide: f32 = natural.iter().filter(|c| **c > share).sum();
    let left = w - narrow;
    natural.iter().map(|c| if *c <= share { *c } else { (c / wide * left).floor() }).collect()
}

/// A table with its top-left at `(x, top)`: a bordered box, the header row
/// tinted and bold, a rule under each row. Returns its height, `None` while
/// any cell is unshaped.
fn paint_table(
    cx: &mut PaintCx,
    cells: &[Vec<Text>],
    cols: &[f32],
    (x, top): (f32, f32),
    clip: Rect,
    look: &Look,
    links: Links,
) -> Option<f32> {
    let (px, py) = cell_pad(look);
    let hair = theme::hairline(cx.ui);
    let width: f32 = cols.iter().sum();
    let mut y = top;
    let mut shaped = true;
    let mut rules = Vec::new();
    let mut head_h = 0.0;
    for (r, row) in cells.iter().enumerate() {
        let mut row_h: f32 = look.line_h;
        let mut cx_x = x;
        for (c, text) in row.iter().enumerate() {
            let cw = cols.get(c).copied().unwrap_or(0.0);
            match paint_text(cx, text, (cx_x + px, y + py, (cw - 2.0 * px).max(1.0)), clip, look, links) {
                Some(h) => row_h = row_h.max(h),
                None => {
                    shaped = false;
                    row_h = row_h.max(estimate_height(&text.spans, cw - 2.0 * px));
                }
            }
            cx_x += cw;
        }
        y += row_h + 2.0 * py;
        if r == 0 {
            head_h = y - top;
        }
        if r + 1 < cells.len() {
            rules.push((y, r == 0));
        }
    }
    let rect = Rect { x: x.round(), y: top.round(), w: width.round(), h: (y - top).round() };
    let radius = theme::box_radius(cx.ui);
    let border = Some((hair, theme::CODE_BOX_BORDER, theme::CODE_BOX_BORDER_ALPHA * 1.5));
    cx.with_clip(clip, |cx| {
        cx.round_rect(rect, radius, theme::CODE_BOX_BG, 0.0, border);
        let head = Rect { h: head_h.round(), ..rect };
        if let Some(head) = intersect(head, clip) {
            cx.with_clip(head, |cx| cx.round_rect(rect, radius, theme::CODE_BOX_BG, theme::CODE_BOX_ALPHA, None));
        }
        for (ry, under_head) in rules {
            let h = if under_head { 2.0 * hair } else { hair };
            let r = Rect { x: rect.x + hair, y: ry.round(), w: rect.w - 2.0 * hair, h };
            cx.fill_alpha(r, theme::CODE_BOX_BORDER, theme::CODE_BOX_BORDER_ALPHA * 1.5);
        }
    });
    shaped.then_some(y - top)
}

/// The first line of the code block whose copy button shows a check mark
/// now, from the editor's last copy (its block's first line, and until
/// when).
pub fn copied_line(copied: Option<(usize, Instant)>) -> Option<usize> {
    copied.filter(|(_, until)| Instant::now() < *until).map(|(l, _)| l)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::testing::{cmd, press, tmpdir, Harness};
    use crate::plugin::{MouseButton, MouseEvent, TextSrc, View};
    use crate::plugins::editor::{EditorBuffer, EditorView, ViewMode, BTN_COPY_BLOCK, BTN_VIEW_MODE};

    const RECT: Rect = Rect { x: 0.0, y: 0.0, w: 500.0, h: 300.0 };

    /// A Markdown document shown in Preview, and its file.
    fn previewed(tag: &str, text: &str) -> (EditorView, PathBuf) {
        let path = tmpdir(tag).join("doc.md");
        std::fs::write(&path, text).unwrap();
        let mut v = EditorView::new(EditorBuffer::open(path.clone()).unwrap());
        v.set_view_mode(ViewMode::Preview);
        (v, path)
    }

    /// The blocks drawn: (text, left, top), top to bottom.
    fn drawn(h: &Harness) -> Vec<(String, f32, f32)> {
        h.frame
            .text
            .iter()
            .filter(|t| matches!(t.src, TextSrc::Block(_)))
            .map(|t| {
                let b = h.text.buffer(t.src).unwrap();
                (b.layout_runs().map(|r| r.text).collect::<Vec<_>>().join("\n"), t.left, t.top)
            })
            .collect()
    }

    fn top_of(h: &Harness, text: &str) -> f32 {
        drawn(h).into_iter().find(|d| d.0 == text).unwrap_or_else(|| panic!("{text:?} in {:?}", drawn(h))).2
    }

    /// The glyph sizes of the drawn block reading `text`.
    fn glyph_sizes(h: &Harness, text: &str) -> Vec<f32> {
        let item = h.frame.text.iter().find(|t| {
            matches!(t.src, TextSrc::Block(_)) && h.text.buffer(t.src).unwrap().layout_runs().any(|r| r.text == text)
        });
        let b = h.text.buffer(item.expect("drawn").src).unwrap();
        b.layout_runs().flat_map(|r| r.glyphs.iter().map(|g| g.font_size)).collect()
    }

    fn down(pos: (f32, f32), clicks: u32) -> MouseEvent {
        MouseEvent::Down { pos, clicks, button: MouseButton::Left }
    }

    #[test]
    fn the_preview_draws_serif_body_text_and_headings_by_level() {
        let (mut v, _) = previewed("pv-sizes", "# Title\n\nBody text.\n\n### Third\n");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let body = (14.0 * BODY_SCALE).round();
        assert!(glyph_sizes(&h, "Body text.").iter().all(|s| *s == body));
        assert!(glyph_sizes(&h, "Title").iter().all(|s| *s == (body * 1.8).round()));
        assert!(glyph_sizes(&h, "Third").iter().all(|s| *s == (body * 1.25).round()));
        let fonts: Vec<Font> = (0..3)
            .map(|i| match v.preview.laid[i].prep.as_ref().unwrap() {
                Prep::Text { text, .. } => text.spans[0].font,
                p => panic!("{p:?}"),
            })
            .collect();
        assert_eq!(fonts, [Font::SERIF.bold(true), Font::SERIF, Font::SERIF.bold(true)]);
        let (t1, t2) = (top_of(&h, "Title"), top_of(&h, "Body text."));
        assert!(t2 > t1 + body * 1.8, "the body follows the heading's height");
    }

    #[test]
    fn inline_code_sits_on_a_pill_under_its_tagged_glyphs() {
        let (mut v, _) = previewed("pv-code", "Run `mise exec` now.\n");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let item = h.frame.text.iter().find(|t| matches!(t.src, TextSrc::Block(_))).unwrap();
        let b = h.text.buffer(item.src).unwrap();
        let code: Vec<f32> =
            b.layout_runs().flat_map(|r| r.glyphs.iter().filter(|g| g.metadata == 1).map(|g| g.x)).collect();
        assert_eq!(code.len(), "mise exec".len(), "the code span keeps its tag");
        let (x0, x1) = (item.left + code[0], item.left + code[code.len() - 1]);
        let (sw, sh) = h.frame.screen;
        let pills: Vec<(f32, f32, f32)> = h
            .frame
            .bg
            .iter()
            .filter(|q| q.radius > 0.0 && q.border_w > 0.0)
            .map(|q| ((q.pos[0] + 1.0) / 2.0 * sw, q.size[0] / 2.0 * sw, (1.0 - q.pos[1]) / 2.0 * sh))
            .collect();
        assert_eq!(pills.len(), 1, "{pills:?}");
        let (px, pw, py) = pills[0];
        assert!(px < x0 && px + pw > x1, "the pill covers the code: {pills:?} vs {x0}..{x1}");
        assert!(py >= item.top && py < item.top + 30.0);
        // The span is monospace at the editor's size.
        let Prep::Text { text, .. } = v.preview.laid[0].prep.as_ref().unwrap() else { panic!() };
        assert_eq!((text.spans[1].font, text.spans[1].size), (Font::MONO, 14.0));
    }

    #[test]
    fn a_link_opens_its_file_beside_the_document_or_its_url_on_release() {
        let (mut v, path) = previewed("pv-link", "See [goal](GOAL.md) or [web](https://x.org/a).\n");
        std::fs::write(path.parent().unwrap().join("GOAL.md"), "# Goal\n").unwrap();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let links: Vec<(Rect, u32)> = h.frame.view_buttons.iter().filter(|b| b.2 >= BTN_LINK).map(|b| (b.0, b.2)).collect();
        assert_eq!(links.len(), 2);
        let out = h.button(&mut v, links[0].1);
        assert_eq!(out.requests, [Request::OpenFile(path.parent().unwrap().join("GOAL.md"))]);
        let out = h.button(&mut v, links[1].1);
        assert_eq!(out.requests, [Request::OpenUrl("https://x.org/a".into())]);
        // Hovering one highlights it.
        let quads = h.frame.bg.len();
        let r = links[0].0;
        h.cursor = (r.x + 2.0, r.y + 2.0);
        h.paint(&mut v, RECT);
        assert!(h.frame.bg.len() > quads);
        assert_eq!(h.frame.cursor.map(|c| c.1), Some(CursorIcon::Pointer));
    }

    #[test]
    fn a_link_to_another_scheme_has_no_button_and_says_why() {
        let (mut v, _) = previewed("pv-blocked", "[app](file:///tmp) [s](smb://h/x) and more\n");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let links: Vec<u32> = h.frame.view_buttons.iter().filter(|b| b.2 >= BTN_LINK).map(|b| b.2).collect();
        assert_eq!(links.len(), 1, "only the file: link opens");
        assert_eq!(h.button(&mut v, links[0]).requests, [Request::ShowInSidebar("/tmp".into())], "a folder");
        let item = h.frame.text.iter().find(|t| matches!(t.src, TextSrc::Block(_))).unwrap();
        let b = h.text.buffer(item.src).unwrap();
        let g = b.layout_runs().flat_map(|r| r.glyphs.iter().cloned()).find(|g| g.metadata == 2).unwrap();
        h.cursor = (item.left + g.x + 1.0, item.top + 4.0);
        h.paint(&mut v, RECT);
        assert_eq!(h.frame.tooltip.as_ref().map(|t| t.1.as_str()), Some("Not opened: smb: links"));
        assert!(h.frame.view_buttons.iter().all(|b| b.2 < BTN_LINK || b.2 == links[0]));
    }

    #[test]
    fn a_folder_link_shows_in_the_sidebar_and_a_device_or_missing_file_opens_nothing() {
        let (mut v, path) = previewed("pv-dir", "[d](sub) [z](/dev/zero) [m](missing.md)\n");
        let dir = path.parent().unwrap().join("sub");
        std::fs::create_dir_all(&dir).unwrap();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let links: Vec<u32> = h.frame.view_buttons.iter().filter(|b| b.2 >= BTN_LINK).map(|b| b.2).collect();
        assert_eq!(links.len(), 3);
        assert_eq!(h.button(&mut v, links[0]).requests, [Request::ShowInSidebar(dir)]);
        assert!(h.button(&mut v, links[1]).requests.is_empty(), "/dev/zero never opens");
        assert!(h.button(&mut v, links[2]).requests.is_empty());
    }

    #[test]
    fn keys_scroll_the_preview_and_never_edit_the_text() {
        let text: String = (0..80).map(|i| format!("Paragraph {i}.\n\n")).collect();
        let (mut v, _) = previewed("pv-keys", &text);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let before = v.text();
        for k in [Key::Char('x'), Key::Backspace, Key::Enter, Key::Tab, Key::Delete] {
            let (used, _) = h.key(&mut v, press(k));
            assert!(!used, "{k:?}");
        }
        assert!(!h.key(&mut v, cmd(Key::Char('v'))).0, "no paste");
        assert_eq!(v.text(), before);
        assert!(!v.buf.modified);
        let top = |h: &Harness| drawn(h)[0].0.clone();
        assert_eq!(top(&h), "Paragraph 0.");
        let (used, out) = h.key(&mut v, press(Key::PageDown));
        assert!(used && out.redraw);
        h.paint(&mut v, RECT);
        assert_ne!(top(&h), "Paragraph 0.");
        h.key(&mut v, press(Key::End));
        h.paint(&mut v, RECT);
        assert_eq!(drawn(&h).last().unwrap().0, "Paragraph 79.");
        h.key(&mut v, press(Key::Home));
        h.paint(&mut v, RECT);
        assert_eq!(top(&h), "Paragraph 0.");
        // The wheel too: content comes up as the wheel turns down.
        h.scroll(&mut v, (10.0, 10.0), -100.0);
        h.paint(&mut v, RECT);
        assert!(top_of(&h, "Paragraph 2.") < 100.0);
    }

    #[test]
    fn the_mode_button_steps_a_markdown_file_through_all_three() {
        let (mut v, _) = previewed("pv-cycle", "# a\n");
        v.set_view_mode(ViewMode::Integrated);
        let mut seen = Vec::new();
        let h = Harness::new();
        for _ in 0..3 {
            h.button(&mut v, BTN_VIEW_MODE);
            seen.push(v.view_mode());
        }
        assert_eq!(seen, [ViewMode::Source, ViewMode::Preview, ViewMode::Integrated]);
        let (fresh, _) = previewed("pv-default", "# a\n");
        assert_eq!(EditorView::new(fresh.buf).view_mode(), ViewMode::Integrated, "Markdown opens Integrated");
    }

    #[test]
    fn a_double_click_edits_the_source_at_the_blocks_line() {
        let (mut v, _) = previewed("pv-dbl", "# A\n\npara one\n\n- item\n- two\n");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let y = top_of(&h, "two") + 2.0;
        h.mouse(&mut v, down((60.0, y), 1));
        assert_eq!(v.view_mode(), ViewMode::Preview, "a single click stays");
        let out = h.mouse(&mut v, down((60.0, y), 2));
        assert!(out.redraw);
        assert_eq!(v.view_mode(), ViewMode::Integrated);
        assert_eq!(v.buf.cursor, (5, 0));
    }

    #[test]
    fn switching_modes_keeps_the_source_line_at_the_top() {
        let text: String = (0..40).map(|i| format!("## Section {i}\n\nText {i}.\n\n")).collect();
        let (mut v, _) = previewed("pv-switch", &text);
        v.set_view_mode(ViewMode::Integrated);
        v.buf.scroll = 40; // "## Section 10"
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        v.set_view_mode(ViewMode::Preview);
        h.paint(&mut v, RECT);
        let first = drawn(&h)[0].clone();
        assert_eq!(first.0, "Section 10");
        assert!(first.2 >= 0.0 && first.2 < 40.0, "at the top: {first:?}");
        h.scroll(&mut v, (10.0, 10.0), -300.0);
        h.paint(&mut v, RECT);
        let line = v.preview.top_line().unwrap();
        v.set_view_mode(ViewMode::Source);
        assert_eq!(v.buf.scroll, line);
        assert!(line > 40);
    }

    #[test]
    fn the_top_block_stays_put_when_blocks_above_it_change_height() {
        let text: String = (0..30).map(|i| format!("Paragraph {i} with some words in it.\n\n")).collect();
        let (mut v, _) = previewed("pv-anchor", &text);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.scroll(&mut v, (10.0, 10.0), -250.0);
        h.paint(&mut v, RECT);
        let anchor = v.preview.anchor;
        assert!(anchor.index > 2);
        let name = format!("Paragraph {} with some words in it.", anchor.index);
        let y = top_of(&h, &name);
        // Every block above re-measures (an estimate, then shaped) far off
        // what it was.
        for l in &mut v.preview.laid[..anchor.index] {
            l.height = Some((500.0, false));
        }
        h.paint(&mut v, RECT);
        assert_eq!(top_of(&h, &name), y);
        assert_eq!(v.preview.anchor, anchor);
        // Scrolling back up then walks the new heights.
        h.scroll(&mut v, (10.0, 10.0), 10.0);
        h.paint(&mut v, RECT);
        assert_eq!(top_of(&h, &name), y + 10.0);
    }

    /// A Preview of 30 wrapping paragraphs, scrolled `dy` px down.
    fn scrolled(tag: &str, dy: f32) -> (EditorView, Harness) {
        let text: String = (0..30).map(|i| format!("Paragraph {i} with enough words in it to wrap at a narrower width.\n\n")).collect();
        let (mut v, _) = previewed(tag, &text);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.scroll(&mut v, (10.0, 10.0), -dy);
        h.paint(&mut v, RECT);
        (v, h)
    }

    #[test]
    fn the_anchor_block_measured_off_its_estimate_keeps_its_bottom_part_in_place() {
        let (mut v, mut h) = scrolled("pv-anchor-self", 260.0);
        let Anchor { index, offset } = v.preview.anchor;
        assert!(offset > 0.0);
        let measured = v.preview.laid[index].height.unwrap().0;
        // As if scrolled up into it while only estimated, 20 px short.
        v.preview.laid[index].height = Some((measured - 20.0, false));
        h.paint(&mut v, RECT);
        assert_eq!(v.preview.anchor, Anchor { index, offset: offset + 20.0 });
        assert!(h.frame.wants_frame, "repaints at the new offset");
    }

    #[test]
    fn a_resize_keeps_the_anchor_blocks_share_above_the_top() {
        let (mut v, mut h) = scrolled("pv-anchor-resize", 260.0);
        let Anchor { index, offset } = v.preview.anchor;
        let share = offset / v.preview.laid[index].height.unwrap().0;
        let narrow = Rect { w: 260.0, ..RECT };
        h.paint(&mut v, narrow);
        h.paint(&mut v, narrow);
        let a = v.preview.anchor;
        assert_eq!(a.index, index);
        let (hgt, measured) = v.preview.laid[index].height.unwrap();
        assert!(measured);
        assert!((a.offset / hgt - share).abs() < 0.01, "{} vs {share}", a.offset / hgt);
    }

    #[test]
    fn a_press_after_a_reparse_never_reads_a_stale_block() {
        let (mut v, mut h) = scrolled("pv-stale", 0.0);
        let y = v.preview.shown.last().unwrap().1 + 1.0;
        v.preview.adopt(parse::parse("one"), 99);
        assert_eq!(v.preview.press((60.0, y), 2), None);
        h.paint(&mut v, RECT);
    }

    #[test]
    fn a_line_asked_for_is_the_top_line_before_it_shows() {
        let mut p = Preview::new(None);
        p.show_line(7);
        assert_eq!(p.top_line(), Some(7));
    }

    #[test]
    fn edits_during_a_parse_coalesce_into_one_more() {
        let text: String = (0..4000).map(|i| format!("Paragraph number {i}.\n\n")).collect();
        let (mut v, _) = previewed("pv-coalesce", &text);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let first = v.preview.job.as_ref().map(|j| j.0);
        assert_eq!(first, Some(v.buf.edits));
        for _ in 0..3 {
            v.write_input(b"x");
            h.paint(&mut v, RECT);
            if let (Some(j), None) = (&v.preview.job, v.preview.parsed) {
                assert_eq!(Some(j.0), first, "no second parse while the first runs");
            }
        }
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !v.preview.is_current(v.buf.edits) {
            v.refresh();
            h.paint(&mut v, RECT);
            assert!(Instant::now() < deadline, "never caught up");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn a_code_block_is_boxed_with_its_language_and_copies_its_body() {
        let (mut v, _) = previewed("pv-fence", "text\n\n```toml\nname = 1 # one\n```\n");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        assert!(h.texts().iter().any(|t| t == "toml"), "the language label: {:?}", h.texts());
        assert!(h.texts().iter().any(|t| t == "name = 1 # one"), "a monospace row");
        let copy: Vec<(Rect, u32)> =
            h.frame.view_buttons.iter().filter(|b| (BTN_COPY_BLOCK..BTN_LINK).contains(&b.2)).map(|b| (b.0, b.2)).collect();
        assert_eq!(copy.iter().map(|c| c.1).collect::<Vec<_>>(), [BTN_COPY_BLOCK + 2], "keyed by its fence's line");
        h.button(&mut v, copy[0].1);
        assert_eq!(h.clipboard.borrow().as_deref(), Some("name = 1 # one"));
        h.cursor = (copy[0].0.x + 2.0, copy[0].0.y + 2.0);
        h.paint(&mut v, RECT);
        assert_eq!(h.frame.tooltip.as_ref().map(|t| t.1.as_str()), Some("Copied"), "a check mark after the copy");
    }

    #[test]
    fn tables_quotes_lists_and_rules_draw() {
        let text = "| Chord | Action |\n|---|---|\n| Cmd+N | new |\n\n> quoted\n\n- [x] done\n1. one\n\n---\n";
        let (mut v, _) = previewed("pv-misc", text);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let texts: Vec<String> = drawn(&h).into_iter().map(|d| d.0).collect();
        for want in ["Chord", "Action", "Cmd+N", "new", "quoted", "done", "1.", "one"] {
            assert!(texts.iter().any(|t| t == want), "{want} in {texts:?}");
        }
        let left = |t: &str| drawn(&h).into_iter().find(|d| d.0 == t).unwrap().1;
        assert!(left("Action") > left("Chord") + 40.0, "columns");
        assert!(left("quoted") > left("Chord"), "a quote indents past its bar");
        assert!(left("done") > left("Chord"), "an item indents past its marker");
    }

    #[test]
    fn front_matter_shows_dim_in_a_code_box() {
        let (mut v, _) = previewed("pv-front", "---\ntitle: A\n---\n# Body\n");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let row = h.frame.text.iter().find(|t| matches!(t.src, TextSrc::Row(_)) && h.text.buffer(t.src).unwrap().layout_runs().any(|r| r.text == "title: A"));
        assert!(row.is_some(), "a monospace row: {:?}", h.texts());
        assert_eq!(drawn(&h).iter().map(|d| d.0.as_str()).collect::<Vec<_>>(), ["Body"], "no rule, no extra heading");
    }

    #[test]
    fn the_agent_reads_the_text_whatever_the_mode() {
        let (mut v, _) = previewed("pv-agent", "# a\n\nb `c`\n");
        let want = v.text();
        assert_eq!(want.as_deref(), Some("# a\n\nb `c`"));
        for m in ViewMode::ALL {
            v.set_view_mode(m);
            assert_eq!(v.text(), want);
        }
    }

    #[test]
    fn a_long_document_parses_on_a_worker() {
        let text: String = (0..4000).map(|i| format!("Paragraph number {i}.\n\n")).collect();
        assert!(text.len() > PARSE_INLINE_MAX);
        let (mut v, _) = previewed("pv-worker", &text);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        // `refresh` also hears other background work (the header's
        // statistic): wait for the parse itself.
        while !v.preview.is_current(v.buf.edits) {
            v.refresh();
            assert!(Instant::now() < deadline, "the parse never finished");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        h.paint(&mut v, RECT);
        assert_eq!(drawn(&h)[0].0, "Paragraph number 0.");
        assert_eq!(v.preview.blocks.len(), 4000);
    }

    #[test]
    fn links_go_to_files_beside_the_document_or_web_and_mail_urls_only() {
        let doc = Path::new("/w/docs/a.md");
        let to = |u: &str| target(u, Some(doc));
        let web = |u: &str| Some(Target::Web(u.into()));
        let file = |p: &str| Some(Target::File(p.into()));
        let blocked = |s: &str| Some(Target::Blocked(s.into()));
        assert_eq!(to("https://x.org/a"), web("https://x.org/a"));
        assert_eq!(to("HTTP://x.org"), web("HTTP://x.org"));
        assert_eq!(to("mailto:me@x.org"), web("mailto:me@x.org"));
        assert_eq!(to("GOAL.md"), file("/w/docs/GOAL.md"));
        assert_eq!(to("../src/my%20file.rs#L3"), file("/w/docs/../src/my file.rs"));
        assert_eq!(to("/etc/hosts"), file("/etc/hosts"));
        // A `file:` URL is a path the app opens itself, never the system.
        assert_eq!(to("file:///Applications/Calculator.app"), file("/Applications/Calculator.app"));
        assert_eq!(to("file://localhost/tmp/a%20b.md"), file("/tmp/a b.md"));
        assert_eq!(to("file:relative"), blocked("file"));
        for (url, scheme) in [("smb://host/s", "smb"), ("SSH://h", "ssh"), ("vscode://x", "vscode"), ("javascript:x", "javascript")] {
            assert_eq!(to(url), blocked(scheme), "{url}");
        }
        // Not a scheme (it starts with no letter): a relative path.
        assert_eq!(to("-a:x"), file("/w/docs/-a:x"));
        assert_eq!(to("#heading"), Some(Target::InPage));
        assert_eq!(target("a.md", None), None, "a scratch buffer has no folder");
        assert_eq!(link_tip("ssh://h", Some(doc)), "Not opened: ssh: links");
    }

    #[test]
    fn a_bad_escape_stays_as_written() {
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("a%zz%41"), "a%zzA");
    }
}
