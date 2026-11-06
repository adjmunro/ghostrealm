//! The undo panel: a column on the editor's right showing its file's undo
//! history ([`UndoTree`]) as a graph, newest step at the top, a lane per
//! branch. The current step is highlighted and the saved one marked; the
//! steps undo passes through draw brighter than other branches'. A click on
//! a step, or Up/Down while the panel has the keys, moves the text there
//! ([`super::EditorBuffer::goto`]).
//!
//! The view owns the buffer and the jump; this keeps the graph, the scroll,
//! and the geometry of the last paint.

use std::collections::{HashMap, HashSet};

use ghostrealm_core::Rect;
use ghostrealm_terminal::{Key, KeyPress};

use super::history::{end_of, NodeId, Op, UndoTree};
use crate::plugin::icons::Icon;
use crate::plugin::{rect_contains, theme, Font, PaintCx, UiMetrics};

/// The panel's width, in cells, at most half the editor's.
const PANEL_CELLS: f32 = 30.0;
/// A lane's width, in cells.
const LANE_CELLS: f32 = 1.2;
/// The graph's widest share of the panel; lanes past it are cut off.
const GRAPH_SHARE: f32 = 0.4;
/// A step's label shows at most this many characters of its text.
const LABEL_CHARS: usize = 40;

/// One step's row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub id: NodeId,
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
    pub fn of(t: &UndoTree) -> Graph {
        let node = |id: NodeId| t.node(id).expect("a step of the tree");
        let mut ids = vec![t.root()];
        let mut i = 0;
        while i < ids.len() {
            ids.extend_from_slice(node(ids[i]).children());
            i += 1;
        }
        // Later steps have larger ids.
        ids.sort_unstable_by(|a, b| b.cmp(a));
        let row_of: HashMap<NodeId, usize> = ids.iter().enumerate().map(|(r, id)| (*id, r)).collect();
        // A line: a step, then its oldest child, its oldest child's, ...
        let mut lines: Vec<Vec<NodeId>> = Vec::new();
        let mut starts = vec![t.root()];
        while let Some(start) = starts.pop() {
            let mut line = vec![start];
            loop {
                let kids = node(*line.last().expect("not empty")).children();
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
            let bottom = match node(line[0]).parent() {
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
            at = node(id).parent();
        }
        let rows = ids
            .iter()
            .enumerate()
            .map(|(r, id)| Row {
                id: *id,
                lane: lane_of[r],
                parent: node(*id).parent().map(|p| row_of[&p]),
                on_path: path.contains(id),
            })
            .collect();
        Graph { rows, lanes: taken.len(), current: row_of[&t.current()], saved: t.saved().map(|s| row_of[&s]) }
    }
}

/// What a tree's graph is drawn from: its root, current and saved steps, and
/// how many steps it has.
type GraphKey = (NodeId, NodeId, Option<NodeId>, usize);

/// It changes with a step added or pruned, an undo, or a save.
fn graph_key(t: &UndoTree) -> GraphKey {
    (t.root(), t.current(), t.saved(), t.steps())
}

/// What step `id` of `t` changed, in short: `+text` inserted, `−text`
/// deleted, `old → new` replaced, or how many changes a step of several
/// made, counted after [`merged`]. The root, the oldest text kept, is
/// "Start".
pub fn label(t: &UndoTree, id: NodeId) -> String {
    let Some(n) = t.node(id) else { return String::new() };
    if n.parent().is_none() {
        return "Start".into();
    }
    match merged(n.ops()).as_slice() {
        [] => String::new(),
        [op] if op.del.is_empty() => format!("+{}", snippet(&op.ins)),
        [op] if op.ins.is_empty() => format!("\u{2212}{}", snippet(&op.del)),
        [op] => format!("{} \u{2192} {}", snippet(&op.del), snippet(&op.ins)),
        ops => format!("{} changes", ops.len()),
    }
}

/// `ops` with neighbours joined, since typing records a change per
/// character: an insert where the last change's inserted text ends (a typed
/// run, or typing over a selection, whose delete inserts nothing), and a
/// delete beside the last delete, after it (Delete) or before it
/// (Backspace).
fn merged(ops: &[Op]) -> Vec<Op> {
    let mut out: Vec<Op> = Vec::new();
    for op in ops {
        let Some(m) = out.last_mut() else {
            out.push(op.clone());
            continue;
        };
        let deletes = op.ins.is_empty() && m.ins.is_empty();
        if op.del.is_empty() && op.at == end_of(m.at, &m.ins) {
            m.ins.push_str(&op.ins);
        } else if deletes && op.at == m.at {
            m.del.push_str(&op.del);
        } else if deletes && end_of(op.at, &op.del) == m.at {
            m.del.insert_str(0, &op.del);
            m.at = op.at;
        } else {
            out.push(op.clone());
        }
    }
    out
}

/// `s` on one line, at most [`LABEL_CHARS`] long: a newline shows as `⏎`.
fn snippet(s: &str) -> String {
    let mut out: String = s.chars().take(LABEL_CHARS).map(|c| if c == '\n' { '\u{23ce}' } else { c }).collect();
    if s.chars().nth(LABEL_CHARS).is_some() {
        out.push('\u{2026}');
    }
    out
}

/// What a key does in the panel.
#[derive(Debug, PartialEq, Eq)]
pub enum PanelKey {
    /// Not the panel's: the text takes it.
    Pass,
    /// The panel took it.
    Took,
    /// Move the text to this step.
    Goto(NodeId),
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
    shown: Option<NodeId>,
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

    /// Its width in an editor `area_w` wide: none while closed.
    pub fn width(&self, ui: UiMetrics, area_w: f32) -> f32 {
        if self.open {
            (PANEL_CELLS * ui.cell_w).min(area_w * 0.5).round()
        } else {
            0.0
        }
    }

    pub fn contains(&self, (x, y): (f32, f32)) -> bool {
        self.rect.is_some_and(|r| rect_contains(r, x, y))
    }

    /// Bring the graph up to date with `t`.
    pub fn update(&mut self, t: &UndoTree) -> &Graph {
        let key = graph_key(t);
        if self.graph.as_ref().is_none_or(|(k, _)| *k != key) {
            self.graph = Some((key, Graph::of(t)));
        }
        &self.graph.as_ref().expect("just built").1
    }

    /// The step on row `i`, as last painted.
    pub fn step(&self, i: u32) -> Option<NodeId> {
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

    /// Draw the panel of `t` over `area`: a title with a close button
    /// (`close`), then the graph, row `i` a button `base + i`.
    pub fn paint(&mut self, cx: &mut PaintCx, area: Rect, t: &UndoTree, base: u32, close: u32) {
        self.update(t);
        self.rect = Some(area);
        let ui = cx.ui;
        let (cw, ch) = (ui.cell_w, ui.cell_h);
        let hair = theme::hairline(ui);
        let pad = (cw * 0.5).round();
        let row_h = (ch * 1.4).round();
        cx.fill_alpha(area, theme::HOVER_BG, 0.03);
        cx.fill(Rect { w: hair, ..area }, theme::INPUT_BORDER);
        if self.focused && cx.focused {
            cx.border(area, theme::FIELD_FOCUS_BORDER);
        }
        // The title row, its close button at the right.
        let title = Rect { h: row_h.min(area.h), ..area };
        let text_top = |top: f32| top + ((row_h - ch) * 0.5).round();
        cx.label("Undo history", Font::SANS, area.x + pad, text_top(title.y), title, theme::TITLE);
        let b = Rect { x: area.x + area.w - row_h, y: title.y, w: row_h, h: row_h };
        let hov = cx.button(b, close);
        if hov {
            cx.round_rect(b, theme::box_radius(ui), theme::HOVER_BG, theme::HOVER_ALPHA, None);
        }
        cx.tooltip(b, "Close undo history");
        let side = (ch * 0.6).round();
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
        let graph_w = (g.lanes as f32 * lane_w).min(((area.w - 2.0 * pad) * GRAPH_SHARE).round());
        let gx = area.x + pad;
        let x_of = |lane: usize| gx + lane as f32 * lane_w + (lane_w * 0.5).round();
        let y_of = |row: usize| inner.y + row as f32 * row_h - scroll + (row_h * 0.5).round();
        let accent = cx.chrome.accent;
        let stroke = (1.5 * ui.scale).round().max(1.0);
        let row_top = |row: usize| (inner.y + row as f32 * row_h - scroll).round();
        cx.with_clip(inner, |cx| {
            for i in first..last {
                let r = Rect { x: area.x + hair, y: row_top(i), w: area.w - hair, h: row_h };
                if i == g.current {
                    cx.fill_alpha(r, theme::ROW_SELECTED_BG, theme::ROW_SELECTED_ALPHA);
                }
                if cx.button(r, base + i as u32) {
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
                if Some(i) == g.saved {
                    let s = cx.shape("saved", Font::SANS, area.w);
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
                cx.row_font(&[(label(t, row.id), color)], Font::SANS, left, top, area.w, clip, color);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::history::{Caret, Op};
    use super::super::{EditorBuffer, EditorView, BTN_LINK, BTN_UNDO, BTN_UNDO_PANEL};
    use super::*;
    use crate::plugin::testing::{cmd, press, tmpdir, Harness, UI};
    use crate::plugin::{MouseButton, MouseEvent, View};
    use ghostrealm_terminal::Mods;

    /// `key` with Shift, typing `text`.
    fn shifted(key: Key, text: Option<&str>) -> KeyPress {
        KeyPress { key, mods: Mods { shift: true, ..Mods::default() }, text: text.map(str::to_string) }
    }

    fn add(t: &mut UndoTree, text: &str) -> NodeId {
        t.push(Caret::default(), usize::MAX);
        t.record(Op { at: (0, 0), del: String::new(), ins: text.into() });
        t.current()
    }

    fn lanes(g: &Graph) -> Vec<usize> {
        g.rows.iter().map(|r| r.lane).collect()
    }

    #[test]
    fn a_line_of_steps_is_one_lane_newest_first() {
        let mut t = UndoTree::default();
        let root = t.root();
        let (a, b) = (add(&mut t, "a"), add(&mut t, "b"));
        let g = Graph::of(&t);
        assert_eq!(g.rows.iter().map(|r| r.id).collect::<Vec<_>>(), [b, a, root]);
        assert_eq!(lanes(&g), [0, 0, 0]);
        assert_eq!(g.rows.iter().map(|r| r.parent).collect::<Vec<_>>(), [Some(1), Some(2), None]);
        assert_eq!((g.current, g.saved, g.lanes), (0, Some(2), 1));
    }

    #[test]
    fn a_branch_takes_the_next_lane_and_undo_s_path_is_marked() {
        let mut t = UndoTree::default();
        let a = add(&mut t, "a");
        let b = add(&mut t, "b");
        t.jump(a);
        let c = add(&mut t, "c");
        let g = Graph::of(&t);
        // Rows: c, b, a, root.
        assert_eq!(g.rows[0].id, c);
        assert_eq!(g.rows[1].id, b);
        assert_eq!(lanes(&g), [1, 0, 0, 0], "a's oldest child keeps its lane");
        assert_eq!(g.rows[0].parent, Some(2));
        let path: Vec<bool> = g.rows.iter().map(|r| r.on_path).collect();
        assert_eq!(path, [true, false, true, true]);
        t.jump(b);
        assert!(Graph::of(&t).rows[1].on_path);
    }

    #[test]
    fn a_lane_frees_once_its_branch_ends() {
        // root → a → b → e → f, c off a, then d off e.
        let mut t = UndoTree::default();
        let a = add(&mut t, "a");
        let b = add(&mut t, "b");
        t.jump(a);
        let c = add(&mut t, "c");
        t.jump(b);
        let e = add(&mut t, "e");
        add(&mut t, "f");
        t.jump(e);
        let d = add(&mut t, "d");
        let g = Graph::of(&t);
        let lane = |id| g.rows.iter().find(|r| r.id == id).unwrap().lane;
        assert_eq!((lane(c), lane(d), lane(e)), (1, 1, 0), "d's rows are above c's");
        assert_eq!(g.lanes, 2);
        // A branch off b spans c's rows: a third lane.
        t.jump(b);
        let x = add(&mut t, "x");
        let g = Graph::of(&t);
        assert_eq!(g.rows.iter().find(|r| r.id == x).unwrap().lane, 2);
    }

    #[test]
    fn a_step_extended_past_a_later_branch_moves_that_branch() {
        // c (lane 1) ended below d's fork; extending c's line past it takes
        // the lane d used.
        let mut t = UndoTree::default();
        let a = add(&mut t, "a");
        let b = add(&mut t, "b");
        t.jump(a);
        let c = add(&mut t, "c");
        t.jump(b);
        let e = add(&mut t, "e");
        let d = add(&mut t, "d");
        t.jump(e);
        let d2 = add(&mut t, "d2");
        let g = Graph::of(&t);
        let lane = |g: &Graph, id| g.rows.iter().find(|r| r.id == id).unwrap().lane;
        assert_eq!((lane(&g, c), lane(&g, d), lane(&g, d2)), (1, 0, 1));
        t.jump(c);
        let c2 = add(&mut t, "c2");
        let g = Graph::of(&t);
        assert_eq!((lane(&g, c), lane(&g, c2), lane(&g, d2)), (1, 1, 2));
    }

    #[test]
    fn labels_say_what_a_step_changed() {
        let mut t = UndoTree::default();
        assert_eq!(label(&t, t.root()), "Start");
        let a = add(&mut t, "hi\nthere");
        assert_eq!(label(&t, a), "+hi\u{23ce}there");
        t.push(Caret::default(), usize::MAX);
        t.record(Op { at: (0, 0), del: "hi".into(), ins: String::new() });
        assert_eq!(label(&t, t.current()), "\u{2212}hi");
        t.record(Op { at: (0, 0), del: "x".into(), ins: "y".into() });
        assert_eq!(label(&t, t.current()), "2 changes");
        let long = add(&mut t, &"z".repeat(50));
        assert_eq!(label(&t, long), format!("+{}\u{2026}", "z".repeat(LABEL_CHARS)));
        t.push(Caret::default(), usize::MAX);
        t.record(Op { at: (0, 0), del: "a".into(), ins: "b".into() });
        assert_eq!(label(&t, t.current()), "a \u{2192} b");
    }

    #[test]
    fn a_run_of_typing_or_deleting_is_one_change() {
        let mut h = Harness::new();
        let mut v = EditorView::new(EditorBuffer::scratch());
        let last = |v: &EditorView| {
            let t = v.buffer().history();
            label(&t, t.current())
        };
        let chars = |s: &str| s.chars().map(Key::Char).collect::<Vec<_>>();
        typed(&h, &mut v, &chars("hello "));
        assert_eq!(last(&v), "+hello ");
        typed(&h, &mut v, &chars("abc"));
        typed(&h, &mut v, &[Key::Backspace; 3]);
        assert_eq!(last(&v), "\u{2212}abc", "a backspace run");
        typed(&h, &mut v, &[Key::Home]);
        typed(&h, &mut v, &[Key::Delete; 3]);
        assert_eq!(last(&v), "\u{2212}hel", "a forward-delete run");
        assert_eq!(lines(&v), ["lo "]);
        h.key(&mut v, cmd(Key::Char('a')));
        typed(&h, &mut v, &chars("xy"));
        assert_eq!(last(&v), "lo  \u{2192} xy", "typing over a selection");
        v.toggle_undo_panel();
        h.paint(&mut v, RECT);
        assert!(h.texts().iter().any(|t| t == "+hello "), "{:?}", h.texts());
    }

    #[test]
    fn keys_walk_the_rows_and_hand_the_rest_to_the_text() {
        let mut t = UndoTree::default();
        let root = t.root();
        let a = add(&mut t, "a");
        let b = add(&mut t, "b");
        let mut p = UndoPanel::default();
        assert_eq!(p.key(&press(Key::Down)), PanelKey::Pass, "closed");
        p.toggle();
        p.update(&t);
        p.page = 2;
        assert_eq!(p.key(&press(Key::Up)), PanelKey::Took, "the newest already");
        assert_eq!(p.key(&press(Key::Down)), PanelKey::Goto(a));
        assert_eq!(p.key(&press(Key::PageDown)), PanelKey::Goto(root));
        assert_eq!(p.key(&press(Key::End)), PanelKey::Goto(root));
        t.jump(root);
        p.update(&t);
        assert_eq!(p.key(&press(Key::Home)), PanelKey::Goto(b));
        p.page = 1;
        assert_eq!(p.key(&press(Key::PageUp)), PanelKey::Goto(a));
        assert_eq!(p.key(&cmd(Key::Char('z'))), PanelKey::Pass, "a chord passes");
        assert!(p.is_focused(), "and keeps the keys");
        let ctrl = KeyPress { mods: Mods { ctrl: true, ..Mods::default() }, ..press(Key::Char('a')) };
        assert_eq!(p.key(&ctrl), PanelKey::Pass);
        assert!(p.is_focused(), "so does a Ctrl chord");
        for k in [
            shifted(Key::Char('A'), Some("A")),
            shifted(Key::Up, None),
            KeyPress { mods: Mods { alt: true, ..Mods::default() }, ..press(Key::Char('=')) },
        ] {
            p.focus(true);
            assert_eq!(p.key(&k), PanelKey::Pass, "{k:?}");
            assert!(!p.is_focused(), "{k:?} types or moves the caret: the text takes the keys");
        }
        p.focus(true);
        assert_eq!(p.key(&press(Key::Escape)), PanelKey::Took);
        assert!(!p.is_focused());
        p.focus(true);
        assert_eq!(p.key(&press(Key::Char('q'))), PanelKey::Pass);
        assert!(!p.is_focused(), "typing gives the keys to the text");
    }

    const RECT: Rect = Rect { x: 0.0, y: 0.0, w: 600.0, h: 300.0 };

    /// The panel's rows as last painted: row index and hit rect.
    fn rows(h: &Harness) -> Vec<(u32, Rect)> {
        let ids = h.frame.view_buttons.iter().filter(|b| (BTN_UNDO..BTN_LINK).contains(&b.2));
        ids.map(|b| (b.2 - BTN_UNDO, b.0)).collect()
    }

    fn typed(h: &Harness, v: &mut EditorView, keys: &[Key]) {
        for k in keys {
            h.key(v, press(*k));
        }
    }

    fn lines(v: &EditorView) -> Vec<String> {
        v.buffer().lines.clone()
    }

    /// A scratch editor with steps "a", a newline, and "b", painted with
    /// its undo panel open.
    fn three_steps(h: &mut Harness) -> EditorView {
        let mut v = EditorView::new(EditorBuffer::scratch());
        typed(h, &mut v, &[Key::Char('a'), Key::Enter, Key::Char('b')]);
        h.button(&mut v, BTN_UNDO_PANEL);
        h.paint(&mut v, RECT);
        v
    }

    #[test]
    fn the_toggle_shows_a_row_per_step_beside_the_text() {
        let mut h = Harness::new();
        let mut v = EditorView::new(EditorBuffer::scratch());
        h.paint(&mut v, RECT);
        assert!(rows(&h).is_empty(), "closed");
        let mut v = three_steps(&mut h);
        assert!(v.undo_panel_open());
        let r = rows(&h);
        assert_eq!(r.len(), 4, "the root and three steps");
        let left = RECT.w - PANEL_CELLS * UI.cell_w;
        assert!(r.iter().all(|(_, b)| b.x >= left), "on the right: {r:?}");
        assert!(h.texts().iter().any(|t| t == "Undo history"));
        assert!(h.texts().iter().any(|t| t == "+b"), "{:?}", h.texts());
        let menu = v.header_menu(&h.cfg);
        assert_eq!(menu.iter().find(|i| i.id == BTN_UNDO_PANEL).and_then(|i| i.checked), Some(true));
        h.button(&mut v, BTN_UNDO_PANEL);
        h.paint(&mut v, RECT);
        assert!(rows(&h).is_empty() && !v.undo_panel_open(), "the close button hides it");
    }

    #[test]
    fn a_click_or_the_arrows_move_the_text_and_an_edit_then_branches() {
        let mut h = Harness::new();
        let mut v = three_steps(&mut h);
        h.button(&mut v, BTN_UNDO + 3);
        assert_eq!(lines(&v), [""], "the oldest row: the text as it started");
        typed(&h, &mut v, &[Key::Up, Key::Up]);
        assert_eq!(lines(&v), ["a", ""], "two steps newer");
        assert_eq!(v.buffer().cursor, (1, 0), "the caret as that step left it");
        typed(&h, &mut v, &[Key::Down, Key::Home]);
        assert_eq!(lines(&v), ["a", "b"]);
        h.button(&mut v, BTN_UNDO + 2);
        // Typing gives the keys back to the text: it edits, adding a branch.
        typed(&h, &mut v, &[Key::Char('x'), Key::Up]);
        assert_eq!(lines(&v), ["ax"]);
        h.paint(&mut v, RECT);
        assert_eq!(rows(&h).len(), 5);
        let g = v.undo.graph.as_ref().unwrap().1.clone();
        assert_eq!((g.lanes, g.rows[0].lane, g.current), (2, 1, 0), "the new step in a lane of its own");
        // The abandoned branch is still there.
        h.button(&mut v, BTN_UNDO + 1);
        assert_eq!(lines(&v), ["a", "b"]);
    }

    #[test]
    fn escape_or_a_click_on_the_text_gives_it_the_keys_and_a_click_on_the_panel_takes_them() {
        let mut h = Harness::new();
        let mut v = three_steps(&mut h);
        typed(&h, &mut v, &[Key::Escape, Key::Up]);
        assert_eq!(lines(&v), ["a", "b"], "Up moved the caret, not the history");
        assert_eq!(v.buffer().cursor, (0, 1));
        // The window takes a press on a row (a button): its click gives the
        // panel the keys.
        h.button(&mut v, BTN_UNDO + 1);
        assert_eq!(lines(&v), ["a", ""]);
        typed(&h, &mut v, &[Key::Down]);
        assert_eq!(lines(&v), ["a"]);
        typed(&h, &mut v, &[Key::Up]);
        assert_eq!(lines(&v), ["a", ""]);
        h.mouse(&mut v, MouseEvent::Down { pos: (10.0, 10.0), button: MouseButton::Left, clicks: 1 });
        typed(&h, &mut v, &[Key::Down]);
        assert_eq!(lines(&v), ["a", ""], "Down moved the caret");
        // A press on the panel beside its buttons, on its title.
        let (_, row) = rows(&h)[0];
        let on_title = (row.x + 4.0, row.y - 4.0);
        let cursor = v.buffer().cursor;
        h.mouse(&mut v, MouseEvent::Down { pos: on_title, button: MouseButton::Left, clicks: 1 });
        h.mouse(&mut v, MouseEvent::Up { pos: on_title, button: MouseButton::Left, dragged: false });
        assert_eq!(v.buffer().cursor, cursor, "a press on the panel isn't the text's");
        typed(&h, &mut v, &[Key::Down]);
        assert_eq!(lines(&v), ["a"]);
    }

    #[test]
    fn a_shifted_or_option_key_types_and_gives_the_text_the_keys() {
        let mut h = Harness::new();
        let mut v = three_steps(&mut h);
        h.key(&mut v, shifted(Key::Char('A'), Some("A")));
        assert_eq!(lines(&v), ["a", "bA"]);
        typed(&h, &mut v, &[Key::Up]);
        assert_eq!(lines(&v), ["a", "bA"], "Up moved the caret, not the history");
        h.paint(&mut v, RECT);
        h.button(&mut v, BTN_UNDO);
        let opt = KeyPress { mods: Mods { alt: true, ..Mods::default() }, key: Key::Char('='), text: Some("\u{2260}".into()) };
        h.key(&mut v, opt);
        assert_eq!(lines(&v), ["a", "bA\u{2260}"], "at the caret the current step left");
        h.paint(&mut v, RECT);
        h.button(&mut v, BTN_UNDO);
        h.key(&mut v, shifted(Key::Left, None));
        assert_eq!(v.buffer().selected_text().as_deref(), Some("\u{2260}"), "Shift+Left selected");
        typed(&h, &mut v, &[Key::Down]);
        assert_eq!(lines(&v), ["a", "bA\u{2260}"], "Down moved the caret");
    }

    #[test]
    fn the_panel_follows_another_views_edits_and_its_jumps_reach_that_view() {
        let tmp = tmpdir("undo-panel-two-views");
        let path = tmp.join("f.txt");
        std::fs::write(&path, "a").unwrap();
        let mut h = Harness::new();
        let mut one = EditorView::new(EditorBuffer::open(path.clone()).unwrap());
        let mut two = EditorView::new(EditorBuffer::open(path).unwrap());
        one.toggle_undo_panel();
        h.paint(&mut one, RECT);
        assert_eq!(rows(&h).len(), 1);
        h.paint(&mut two, RECT);
        typed(&h, &mut two, &[Key::Char('x')]);
        assert!(one.refresh(), "the edit repaints the other view");
        h.paint(&mut one, RECT);
        assert_eq!(rows(&h).len(), 2);
        assert!(h.texts().iter().any(|t| t == "saved"), "the step on disk is marked");
        h.button(&mut one, BTN_UNDO + 1);
        assert!(two.refresh());
        assert_eq!(lines(&two), ["a"]);
        assert!(!two.modified(), "back at the saved step");
    }

    #[test]
    fn the_wheel_over_the_panel_scrolls_its_rows_not_the_text() {
        let mut h = Harness::new();
        let mut v = EditorView::new(EditorBuffer::scratch());
        for _ in 0..60 {
            typed(&h, &mut v, &[Key::Enter]);
        }
        v.toggle_undo_panel();
        h.paint(&mut v, RECT);
        assert_eq!(rows(&h)[0].0, 0, "the current step, the newest, shows");
        let (_, row) = rows(&h)[0];
        let top = v.buffer().scroll;
        h.scroll(&mut v, (row.x + 4.0, row.y + 4.0), -100.0);
        h.paint(&mut v, RECT);
        assert!(rows(&h)[0].0 > 0, "rows scrolled");
        assert_eq!(v.buffer().scroll, top, "the text didn't");
    }
}
