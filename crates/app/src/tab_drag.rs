//! Dragging a pane tab to another pane or a new split: the press, the drag
//! threshold, the drop zone under the pointer, and the drop on release.
//! Geometry is passed in, so the flow is testable without a window.

use ghostrealm_core::{PaneId, Rect, SurfaceId, TabDrop, Tree, VtabId};

/// Where the workspace is laid out and how a drag arms. Physical pixels.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub workspace: Rect,
    /// The gap between split children.
    pub gap: f32,
    /// How far in from the workspace's edge a drop splits the whole workspace.
    pub band: f32,
    /// How far the pointer moves from the press before it drags.
    pub threshold: f32,
}

/// The pane tab being pressed or dragged.
#[derive(Default)]
pub struct TabDrag {
    press: Option<Press>,
}

struct Press {
    vt: VtabId,
    sid: SurfaceId,
    at: (f32, f32),
    /// The pressed pane's tab strip: no drop zone, and the pointer leaving it
    /// starts the drag, so a click with some drift stays a click.
    strip: Rect,
    armed: bool,
    over: Option<TabDrop>,
}

impl TabDrag {
    /// A left press at `at` on tab `idx` of pane `pane`, whose tab strip is
    /// `strip`: the tab becomes the pane's active one, and dragging it may move it.
    pub fn press(&mut self, tree: &mut Tree, vt: VtabId, pane: PaneId, idx: usize, at: (f32, f32), strip: Rect) {
        let Some(p) = tree.vtab_mut(vt).and_then(|v| v.pane_mut(pane)) else {
            return;
        };
        let Some(sid) = p.surfaces.get(idx).map(|s| s.id) else {
            return;
        };
        p.active = idx;
        self.press = Some(Press { vt, sid, at, strip, armed: false, over: None });
    }

    /// The pointer moved to `p` with the button held: past the threshold and
    /// out of the tab strip the drag shows its zones, then tracks the one
    /// under `p`. Returns whether to redraw.
    pub fn moved(&mut self, tree: &Tree, frame: &Frame, p: (f32, f32)) -> bool {
        let Some(d) = self.press.as_mut() else {
            return false;
        };
        let in_strip = contains(d.strip, p);
        let started = !d.armed;
        if started {
            let (dx, dy) = (p.0 - d.at.0, p.1 - d.at.1);
            if dx * dx + dy * dy < frame.threshold * frame.threshold || in_strip {
                return false;
            }
            d.armed = true;
        }
        let now = if in_strip {
            None
        } else {
            tree.vtab(d.vt)
                .filter(|_| tree.active_vtab() == Some(d.vt))
                .and_then(|v| v.tab_drop_at(frame.workspace, frame.gap, frame.band, p))
        };
        let changed = now != d.over;
        d.over = now;
        started || changed
    }

    /// The button came up: a dragged tab over a zone moves there. Returns
    /// whether a press was held (the zones then go).
    pub fn release(&mut self, tree: &mut Tree) -> bool {
        let Some(d) = self.press.take() else {
            return false;
        };
        if let Some(to) = d.over.filter(|_| d.armed) {
            tree.move_surface(d.vt, d.sid, to);
        }
        true
    }

    /// Let go without dropping. Returns whether a press was held.
    pub fn cancel(&mut self) -> bool {
        self.press.take().is_some()
    }

    /// Whether the press holds a tab, so a drag is the tab's to handle.
    pub fn pressed(&self) -> bool {
        self.press.is_some()
    }

    /// While dragging: the workspace, the dragged surface and the zone under
    /// the pointer, for painting.
    pub fn shown(&self) -> Option<(VtabId, SurfaceId, Option<TabDrop>)> {
        self.press.as_ref().filter(|d| d.armed).map(|d| (d.vt, d.sid, d.over))
    }
}

fn contains(r: Rect, p: (f32, f32)) -> bool {
    p.0 >= r.x && p.0 < r.x + r.w && p.1 >= r.y && p.1 < r.y + r.h
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostrealm_core::{Axis, Edge, Node};

    const FRAME: Frame = Frame {
        workspace: Rect { x: 0.0, y: 0.0, w: 1000.0, h: 600.0 },
        gap: 0.0,
        band: 24.0,
        threshold: 6.0,
    };
    const STRIP_H: f32 = 40.0;

    /// Two panes side by side; the left one has two tabs.
    fn side_by_side() -> (Tree, VtabId, PaneId) {
        let mut t = Tree::new();
        let (vt, left, _) = t.add_vtab("w");
        t.add_surface(vt, left).unwrap();
        t.split(vt, left, Axis::LeftRight).unwrap();
        (t, vt, left)
    }

    /// Press the left pane's first tab, at (100, 20) in its strip.
    fn press_first_tab(t: &mut Tree, vt: VtabId, left: PaneId) -> TabDrag {
        let mut drag = TabDrag::default();
        let strip = Rect { x: 0.0, y: 0.0, w: 500.0, h: STRIP_H };
        drag.press(t, vt, left, 0, (100.0, 20.0), strip);
        drag
    }

    #[test]
    fn a_press_with_drift_inside_the_tab_strip_stays_a_click() {
        let (mut t, vt, left) = side_by_side();
        let mut drag = press_first_tab(&mut t, vt, left);
        let before = t.vtab(vt).unwrap().root.clone();
        // Past the threshold, into the top band and the pane's top side, but
        // still on the strip.
        for p in [(108.0, 20.0), (110.0, 4.0), (130.0, 30.0)] {
            assert!(!drag.moved(&t, &FRAME, p));
        }
        assert_eq!(drag.shown(), None);
        assert!(drag.release(&mut t));
        let v = t.vtab(vt).unwrap();
        assert_eq!(v.root, before, "layout unchanged");
        assert_eq!(v.panes()[0].active, 0, "the pressed tab is active");
    }

    #[test]
    fn a_drag_out_of_the_strip_drops_on_the_zone_under_the_pointer() {
        let (mut t, vt, left) = side_by_side();
        // Pressing the second tab makes it active first.
        let mut drag = TabDrag::default();
        drag.press(&mut t, vt, left, 1, (300.0, 20.0), Rect { x: 0.0, y: 0.0, w: 500.0, h: STRIP_H });
        let sid = t.vtab(vt).unwrap().panes()[0].surfaces[1].id;
        assert_eq!(t.vtab(vt).unwrap().panes()[0].active, 1);
        assert!(drag.moved(&t, &FRAME, (300.0, 200.0)), "leaving the strip starts the drag");
        assert_eq!(drag.shown().map(|(_, s, _)| s), Some(sid));
        assert!(drag.moved(&t, &FRAME, (300.0, 590.0)));
        assert_eq!(drag.shown().and_then(|(.., over)| over), Some(TabDrop::Workspace(Edge::Bottom)));
        // Back over the strip is no zone.
        assert!(drag.moved(&t, &FRAME, (300.0, 20.0)));
        assert_eq!(drag.shown().and_then(|(.., over)| over), None);
        drag.moved(&t, &FRAME, (300.0, 590.0));
        assert!(drag.release(&mut t));
        let v = t.vtab(vt).unwrap();
        let Node::Split { axis: Axis::TopBottom, second, .. } = &v.root else {
            panic!("a full-width pane across the bottom");
        };
        assert!(matches!(&**second, Node::Leaf(p) if p.surfaces[0].id == sid));
        assert!(!drag.release(&mut t), "the release ended the press");
    }

    #[test]
    fn a_release_off_every_zone_or_a_cancel_moves_nothing() {
        let (mut t, vt, left) = side_by_side();
        let mut drag = press_first_tab(&mut t, vt, left);
        let before = t.vtab(vt).unwrap().root.clone();
        drag.moved(&t, &FRAME, (100.0, 300.0));
        drag.moved(&t, &FRAME, (100.0, 700.0));
        drag.release(&mut t);
        assert_eq!(t.vtab(vt).unwrap().root, before);
        let mut drag = press_first_tab(&mut t, vt, left);
        drag.moved(&t, &FRAME, (100.0, 590.0));
        assert!(drag.cancel());
        assert!(!drag.release(&mut t));
        assert_eq!(t.vtab(vt).unwrap().root, before);
    }
}
