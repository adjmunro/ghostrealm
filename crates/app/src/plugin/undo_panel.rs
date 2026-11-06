//! The undo panel: an undo history ([`History`]) drawn as a graph, newest
//! step at the top, a lane per branch. The current step is highlighted and
//! the saved one marked; the steps undo passes through draw brighter than
//! other branches'. A click on a step, or Up/Down while the panel has the
//! keys, asks its host to go there. A history may tag a step ([`History::note`])
//! or offer a button on its row ([`History::action`]).
//!
//! The host owns the history, the jump and where the panel goes; this keeps
//! the graph, the scroll, and the geometry of the last paint.

use std::collections::{HashMap, HashSet};

use ghostrealm_core::Rect;
use ghostrealm_terminal::{Key, KeyPress};

use crate::plugin::icons::Icon;
use crate::plugin::{rect_contains, theme, Font, PaintCx, UiMetrics};

/// A lane's width, in cells.
const LANE_CELLS: f32 = 1.2;
/// The graph's widest share of the panel; lanes past it are cut off.
const GRAPH_SHARE: f32 = 0.4;

/// A step's id, unique in its history; later steps have larger ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StepId(pub(crate) u64);

/// A history the panel draws: a tree of steps from a root.
pub trait History {
    fn root(&self) -> StepId;
    /// The step in effect.
    fn current(&self) -> StepId;
    /// The step marked "saved", if any.
    fn saved(&self) -> Option<StepId>;
    /// How many steps it has, the root included.
    fn steps(&self) -> usize;
    fn parent(&self, id: StepId) -> Option<StepId>;
    /// Oldest first.
    fn children(&self, id: StepId) -> &[StepId];
    /// What step `id` did, in short.
    fn label(&self, id: StepId) -> String;
    /// A tag shown at the right of step `id`'s row, like "saved".
    fn note(&self, _id: StepId) -> Option<&'static str> {
        None
    }
    /// The label of a button on step `id`'s row that acts on that step alone.
    fn action(&self, _id: StepId) -> Option<&'static str> {
        None
    }
}

/// The button ids a panel registers: row `i` is `rows + i`, its action
/// button `actions + i`.
#[derive(Clone, Copy, Debug)]
pub struct PanelIds {
    pub rows: u32,
    pub close: u32,
    pub actions: Option<u32>,
}

/// The side of the panel that borders its host's content, drawn as a hairline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Top,
}

/// One step's row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub id: StepId,
    pub lane: usize,
    /// The row of its parent step.
    pub parent: Option<usize>,
    /// The current step or one before it: undo passes it.
    pub on_path: bool,
}

/// The tree's steps as rows, newest first. A step's oldest child carries on
/// its lane; another child starts a line in the lowest lane free from the
/// row above its parent up to its line's newest step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Graph {
    pub rows: Vec<Row>,
    pub lanes: usize,
    /// The current step's row.
    pub current: usize,
    /// The saved step's row.
    pub saved: Option<usize>,
}

impl Graph {
    pub fn of(t: &dyn History) -> Graph {
        let mut ids = vec![t.root()];
        let mut i = 0;
        while i < ids.len() {
            ids.extend_from_slice(t.children(ids[i]));
            i += 1;
        }
        // Later steps have larger ids.
        ids.sort_unstable_by(|a, b| b.cmp(a));
        let row_of: HashMap<StepId, usize> = ids.iter().enumerate().map(|(r, id)| (*id, r)).collect();
        // A line: a step, then its oldest child, its oldest child's, ...
        let mut lines: Vec<Vec<StepId>> = Vec::new();
        let mut starts = vec![t.root()];
        while let Some(start) = starts.pop() {
            let mut line = vec![start];
            loop {
                let kids = t.children(*line.last().expect("not empty"));
                starts.extend_from_slice(kids.get(1..).unwrap_or_default());
                match kids.first() {
                    Some(c) => line.push(*c),
                    None => break,
                }
            }
            lines.push(line);
        }
        lines.sort_unstable_by_key(|l| l[0]);
        // Each lane's taken spans of rows, top and bottom.
        let mut taken: Vec<Vec<(usize, usize)>> = Vec::new();
        let mut lane_of = vec![0; ids.len()];
        for line in &lines {
            let top = row_of[line.last().expect("not empty")];
            // A branch's edge leaves its parent's lane at the row above it.
            let bottom = match t.parent(line[0]) {
                Some(p) => row_of[&p] - 1,
                None => row_of[&line[0]],
            };
            let free = |spans: &Vec<(usize, usize)>| spans.iter().all(|&(t, b)| b < top || bottom < t);
            let lane = taken.iter().position(free).unwrap_or(taken.len());
            if lane == taken.len() {
                taken.push(Vec::new());
            }
            taken[lane].push((top, bottom));
            for id in line {
                lane_of[row_of[id]] = lane;
            }
        }
        let mut path = HashSet::new();
        let mut at = Some(t.current());
        while let Some(id) = at {
            path.insert(id);
            at = t.parent(id);
        }
        let rows = ids
            .iter()
            .enumerate()
            .map(|(r, id)| Row {
                id: *id,
                lane: lane_of[r],
                parent: t.parent(*id).map(|p| row_of[&p]),
                on_path: path.contains(id),
            })
            .collect();
        Graph { rows, lanes: taken.len(), current: row_of[&t.current()], saved: t.saved().map(|s| row_of[&s]) }
    }
}

/// What a tree's graph is drawn from: its root, current and saved steps, and
/// how many steps it has.
type GraphKey = (StepId, StepId, Option<StepId>, usize);

/// It changes with a step added or pruned, an undo, or a save.
fn graph_key(t: &dyn History) -> GraphKey {
    (t.root(), t.current(), t.saved(), t.steps())
}

/// The height of the panel's title and of each of its rows.
pub fn row_height(ui: UiMetrics) -> f32 {
    (ui.cell_h * 1.4).round()
}

/// What a key does in the panel.
#[derive(Debug, PartialEq, Eq)]
pub enum PanelKey {
    /// Not the panel's: the text takes it.
    Pass,
    /// The panel took it.
    Took,
    /// Move the text to this step.
    Goto(StepId),
}

/// An undo panel's state.
#[derive(Default)]
pub struct UndoPanel {
    open: bool,
    /// It takes the arrow keys, until the text is clicked or typed in.
    focused: bool,
    /// The graph and the key it was built for.
    graph: Option<(GraphKey, Graph)>,
    /// How far the rows are scrolled (px).
    scroll: f32,
    /// The current step as last painted: when it changes, its row scrolls
    /// into view.
    shown: Option<StepId>,
    /// The panel as last painted, and how many rows fit in it.
    rect: Option<Rect>,
    page: usize,
}

impl UndoPanel {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn is_focused(&self) -> bool {
        self.open && self.focused
    }

    /// Open it with the keys, or close it.
    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.focused = self.open;
        self.shown = None;
        if !self.open {
            (self.graph, self.rect) = (None, None);
        }
    }

    pub fn focus(&mut self, on: bool) {
        self.focused = on;
    }

    pub fn contains(&self, (x, y): (f32, f32)) -> bool {
        self.rect.is_some_and(|r| rect_contains(r, x, y))
    }

    /// Bring the graph up to date with `t`.
    pub fn update(&mut self, t: &dyn History) -> &Graph {
        let key = graph_key(t);
        if self.graph.as_ref().is_none_or(|(k, _)| *k != key) {
            self.graph = Some((key, Graph::of(t)));
        }
        &self.graph.as_ref().expect("just built").1
    }

    /// The graph as last built.
    pub fn graph(&self) -> Option<&Graph> {
        self.graph.as_ref().map(|(_, g)| g)
    }

    /// How many rows a page key moves (the panel's height in rows when
    /// painted).
    #[cfg(test)]
    pub fn set_page(&mut self, rows: usize) {
        self.page = rows;
    }

    /// The step on row `i`, as last painted.
    pub fn step(&self, i: u32) -> Option<StepId> {
        self.graph.as_ref()?.1.rows.get(i as usize).map(|r| r.id)
    }

    /// What `k` does while the panel has the keys (call [`Self::update`]
    /// first): Up and Down move to the next newer and older step, whatever
    /// its branch, Page Up and Page Down a panel's height, Home and End to
    /// the newest step and the oldest. Escape and Enter give the keys back
    /// to the text. A Cmd or Ctrl chord goes to the text and leaves the keys
    /// here; any other key goes to the text and takes them with it, since
    /// it types or moves the text's caret.
    pub fn key(&mut self, k: &KeyPress) -> PanelKey {
        if !self.is_focused() || k.mods.super_ || k.mods.ctrl {
            return PanelKey::Pass;
        }
        if k.mods.shift || k.mods.alt {
            self.focused = false;
            return PanelKey::Pass;
        }
        let Some((_, g)) = &self.graph else { return PanelKey::Pass };
        let (cur, last, page) = (g.current, g.rows.len() - 1, self.page.max(1));
        let to = match k.key {
            Key::Up => cur.saturating_sub(1),
            Key::Down => (cur + 1).min(last),
            Key::PageUp => cur.saturating_sub(page),
            Key::PageDown => (cur + page).min(last),
            Key::Home => 0,
            Key::End => last,
            Key::Escape | Key::Enter => {
                self.focused = false;
                return PanelKey::Took;
            }
            _ => {
                self.focused = false;
                return PanelKey::Pass;
            }
        };
        match to == cur {
            true => PanelKey::Took,
            false => PanelKey::Goto(g.rows[to].id),
        }
    }

    /// The wheel over the panel scrolls its rows (`dy` positive shows newer
    /// ones). Returns whether it took the wheel.
    pub fn scroll(&mut self, (x, y): (f32, f32), dy: f32) -> bool {
        if !self.contains((x, y)) {
            return false;
        }
        if dy.is_finite() {
            self.scroll = (self.scroll - dy).max(0.0);
        }
        true
    }

    /// Draw the panel of `t` over `area`: a title with a close button, then
    /// the graph, each row a button ([`PanelIds`]).
    pub fn paint(&mut self, cx: &mut PaintCx, area: Rect, t: &dyn History, ids: PanelIds, edge: Edge) {
        self.update(t);
        self.rect = Some(area);
        let ui = cx.ui;
        let (cw, ch) = (ui.cell_w, ui.cell_h);
        let hair = theme::hairline(ui);
        let pad = (cw * 0.5).round();
        let row_h = row_height(ui);
        cx.fill_alpha(area, theme::HOVER_BG, 0.03);
        let line = match edge {
            Edge::Left => Rect { w: hair, ..area },
            Edge::Top => Rect { h: hair, ..area },
        };
        cx.fill(line, theme::INPUT_BORDER);
        if self.focused && cx.focused {
            cx.border(area, theme::FIELD_FOCUS_BORDER);
        }
        // The title row, its close button at the right, both within the
        // panel however narrow.
        let title = Rect { h: row_h.min(area.h), ..area };
        let text_top = |top: f32| top + ((row_h - ch) * 0.5).round();
        let bw = row_h.min(area.w);
        let b = Rect { x: area.x + area.w - bw, y: title.y, w: bw, h: row_h };
        let title_clip = Rect { w: area.w - bw, ..title };
        cx.label("Undo history", Font::SANS, area.x + pad, text_top(title.y), title_clip, theme::TITLE);
        let hov = cx.button(b, ids.close);
        if hov {
            cx.round_rect(b, theme::box_radius(ui), theme::HOVER_BG, theme::HOVER_ALPHA, None);
        }
        cx.tooltip(b, "Close undo history");
        let side = (ch * 0.6).round().min(b.w);
        let inset = ((b.w - side) * 0.5).round();
        let icon = Rect { x: b.x + inset, y: b.y + inset, w: side, h: side };
        Icon::Close.paint(cx, icon, if hov { theme::GLYPH_HOVER } else { theme::GLYPH });

        let inner = Rect { y: area.y + row_h, h: (area.h - row_h).max(0.0), ..area };
        let Some((_, g)) = &self.graph else { return };
        self.page = ((inner.h / row_h).floor() as usize).max(1);
        let current = g.rows[g.current].id;
        let max_scroll = (g.rows.len() as f32 * row_h - inner.h).max(0.0);
        if self.shown != Some(current) {
            self.shown = Some(current);
            let at = g.current as f32 * row_h;
            if at < self.scroll || at + row_h > self.scroll + inner.h {
                self.scroll = at - (inner.h - row_h) * 0.5;
            }
        }
        self.scroll = self.scroll.clamp(0.0, max_scroll);
        let scroll = self.scroll;
        let first = (scroll / row_h).floor() as usize;
        let last = (((scroll + inner.h) / row_h).ceil() as usize).min(g.rows.len());
        let lane_w = (LANE_CELLS * cw).round();
        let graph_w = (g.lanes as f32 * lane_w).min(((area.w - 2.0 * pad).max(0.0) * GRAPH_SHARE).round());
        let gx = area.x + pad;
        let x_of = |lane: usize| gx + lane as f32 * lane_w + (lane_w * 0.5).round();
        let y_of = |row: usize| inner.y + row as f32 * row_h - scroll + (row_h * 0.5).round();
        let accent = cx.chrome.accent;
        let stroke = (1.5 * ui.scale).round().max(1.0);
        let row_top = |row: usize| (inner.y + row as f32 * row_h - scroll).round();
        // Each row's action button, at its right: registered before the
        // row's, so a press on it is its own.
        let mut acts = Vec::new();
        cx.with_clip(inner, |cx| {
            for i in first..last {
                let r = Rect { x: area.x + hair, y: row_top(i), w: area.w - hair, h: row_h };
                if i == g.current {
                    cx.fill_alpha(r, theme::ROW_SELECTED_BG, theme::ROW_SELECTED_ALPHA);
                }
                let act = ids.actions.zip(t.action(g.rows[i].id));
                if let Some((base, label)) = act {
                    let s = cx.shape(label, Font::SANS, area.w);
                    let w = (s.width + 2.0 * pad).min(r.w);
                    let b = Rect { x: r.x + r.w - w, w, ..r };
                    let hov = cx.button(b, base + i as u32);
                    if hov {
                        cx.round_rect(b, theme::box_radius(ui), theme::HOVER_BG, theme::HOVER_ALPHA, None);
                    }
                    acts.push((i, b, label, hov));
                }
                if cx.button(r, ids.rows + i as u32) {
                    cx.highlight(r);
                }
            }
            // An edge from each step down to its parent: across to its own
            // lane in the row above the parent, then straight down.
            let graph = Rect { x: gx, w: graph_w, ..inner };
            cx.with_clip(graph, |cx| {
                for (i, row) in g.rows.iter().enumerate() {
                    let Some(p) = row.parent.filter(|p| *p >= first && i < last) else { continue };
                    let color = if row.on_path { theme::LABEL } else { theme::DIM };
                    let (x, y) = (x_of(row.lane), y_of(i));
                    let turn = y_of(p - 1);
                    if turn > y {
                        cx.line((x, y), (x, turn), stroke, color, 1.0);
                    }
                    cx.line((x, turn.max(y)), (x_of(g.rows[p].lane), y_of(p)), stroke, color, 1.0);
                }
                let dot = (ch * 0.45).round();
                for (i, row) in g.rows.iter().enumerate().take(last).skip(first) {
                    let color = match () {
                        _ if i == g.current => accent,
                        _ if row.on_path => theme::TEXT,
                        _ => theme::DIM,
                    };
                    let c = (x_of(row.lane), y_of(i));
                    let d = Rect { x: (c.0 - dot * 0.5).round(), y: (c.1 - dot * 0.5).round(), w: dot, h: dot };
                    cx.round_rect(d, dot * 0.5, color, 1.0, None);
                }
            });
            let left = gx + graph_w + pad;
            for (i, row) in g.rows.iter().enumerate().take(last).skip(first) {
                let top = text_top(row_top(i));
                let mut right = area.x + area.w - pad;
                if let Some(&(_, b, label, hov)) = acts.iter().find(|a| a.0 == i) {
                    let s = cx.shape(label, Font::SANS, area.w);
                    cx.place(s, b.x + pad, top, inner, if hov { theme::TEXT } else { theme::LABEL });
                    right = b.x;
                }
                let tag = if Some(i) == g.saved { Some("saved") } else { t.note(row.id) };
                if let Some(tag) = tag {
                    let s = cx.shape(tag, Font::SANS, area.w);
                    right -= s.width;
                    cx.place(s, right, top, inner, theme::ON);
                    right -= pad;
                }
                let color = match () {
                    _ if i == g.current => theme::TEXT,
                    _ if row.on_path => theme::LABEL,
                    _ => theme::DIM,
                };
                let clip = Rect { x: left, w: (right - left).max(0.0), ..inner };
                cx.row_font(&[(t.label(row.id), color)], Font::SANS, left, top, area.w, clip, color);
            }
        });
    }
}
