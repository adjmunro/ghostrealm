//! The editor's side of the undo panel ([`crate::plugin::undo_panel`]): its
//! file's history ([`UndoTree`]) as the panel's [`History`], each step
//! labelled by the text it changed, and the panel's width beside the text.

use super::history::{end_of, NodeId, Op, UndoTree};
use crate::plugin::undo_panel::{History, UndoPanel};
use crate::plugin::UiMetrics;

/// The panel's width, in cells, at most half the editor's.
const PANEL_CELLS: f32 = 30.0;
/// A step's label shows at most this many characters of its text.
const LABEL_CHARS: usize = 40;

impl History for UndoTree {
    fn root(&self) -> NodeId {
        UndoTree::root(self)
    }

    fn current(&self) -> NodeId {
        UndoTree::current(self)
    }

    fn saved(&self) -> Option<NodeId> {
        UndoTree::saved(self)
    }

    fn steps(&self) -> usize {
        UndoTree::steps(self)
    }

    fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.node(id).and_then(|n| n.parent())
    }

    fn children(&self, id: NodeId) -> &[NodeId] {
        self.node(id).map_or(&[], |n| n.children())
    }

    fn label(&self, id: NodeId) -> String {
        label(self, id)
    }
}

/// The panel's width in an editor `area_w` wide: none while closed.
pub fn panel_width(p: &UndoPanel, ui: UiMetrics, area_w: f32) -> f32 {
    if p.is_open() {
        (PANEL_CELLS * ui.cell_w).min(area_w * 0.5).round()
    } else {
        0.0
    }
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

#[cfg(test)]
mod tests {
    use super::super::history::{Caret, Op};
    use super::super::{EditorBuffer, EditorView, BTN_LINK, BTN_UNDO, BTN_UNDO_PANEL};
    use super::*;
    use crate::plugin::undo_panel::{Graph, PanelKey, UndoPanel};
    use ghostrealm_core::Rect;
    use ghostrealm_terminal::{Key, KeyPress};
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
        p.set_page(2);
        assert_eq!(p.key(&press(Key::Up)), PanelKey::Took, "the newest already");
        assert_eq!(p.key(&press(Key::Down)), PanelKey::Goto(a));
        assert_eq!(p.key(&press(Key::PageDown)), PanelKey::Goto(root));
        assert_eq!(p.key(&press(Key::End)), PanelKey::Goto(root));
        t.jump(root);
        p.update(&t);
        assert_eq!(p.key(&press(Key::Home)), PanelKey::Goto(b));
        p.set_page(1);
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
        let g = v.undo.graph().unwrap().clone();
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
    fn a_narrow_panel_keeps_its_close_button_inside() {
        let mut h = Harness::new();
        let mut v = three_steps(&mut h);
        for w in [24.0, 6.0, 1.0] {
            let area = Rect { w, ..RECT };
            h.paint(&mut v, area);
            let close = h.frame.view_buttons.iter().find(|b| b.2 == BTN_UNDO_PANEL).expect("a close button").0;
            let left = area.x + area.w - panel_width(&v.undo, UI, area.w);
            assert!(close.x >= left && close.x + close.w <= area.x + area.w, "{w}: {close:?}");
        }
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
