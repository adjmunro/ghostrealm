//! Split dividers under the mouse: the one the pointer is over, the one being
//! dragged, and a double-click to reset a split. Geometry and click counts are
//! passed in, so the press/drag/release flow is testable without a window.

use ghostrealm_core::{Axis, Divider, PaneId, Rect, SplitPath, Tree, Vtab, VtabId};

/// Dragging a divider leaves each pane at least this many columns / rows.
const MIN_PANE_COLS: f32 = 8.0;
const MIN_PANE_ROWS: f32 = 4.0;

/// Where the dividers are laid out, and the cell size that sets how small a
/// drag may make a pane. All in physical pixels.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub workspace: Rect,
    /// The gap between split children.
    pub gap: f32,
    /// How far either side of the gap a pointer still grabs it.
    pub slop: f32,
    pub cell: (f32, f32),
}

/// What a press on a divider did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    /// Grabbed it: moves drag it until the release.
    Grabbed,
    /// A double-click: its split went back to halves.
    Reset,
}

/// The dividers' pointer state.
#[derive(Default)]
pub struct Dividers {
    /// The divider the pointer is over (highlighted, resize cursor).
    hover: Option<(VtabId, SplitPath)>,
    /// The divider being dragged with the left button.
    drag: Option<Drag>,
}

/// A divider being dragged: which split, and where along its strip it was
/// grabbed.
struct Drag {
    vt: VtabId,
    path: SplitPath,
    grab: f32,
    /// The vtab's panes when it was grabbed. A pane closing reshapes the tree,
    /// and `path` may then name another split (or another axis).
    panes: Vec<PaneId>,
}

impl Dividers {
    /// Whether a divider is grabbed.
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Track the divider the pointer is over, `over`; a drag keeps the one it
    /// holds. Returns whether it changed.
    pub fn hover(&mut self, over: Option<&(VtabId, Divider)>) -> bool {
        if self.drag.is_some() {
            return false;
        }
        let key = over.map(|(vt, d)| (*vt, d.path.clone()));
        if key == self.hover {
            return false;
        }
        self.hover = key;
        true
    }

    /// A left press on divider `d` of `vt`, the `clicks`th of a multi-click:
    /// grab it to drag, or reset its split to halves on a double-click.
    pub fn press(&mut self, tree: &mut Tree, (vt, d): (VtabId, Divider), p: (f32, f32), clicks: u32) -> Press {
        if clicks >= 2 {
            tree.set_ratio(vt, &d.path, 0.5);
            return Press::Reset;
        }
        let grab = d.grab_offset(p);
        let panes = tree.vtab(vt).map(pane_ids).unwrap_or_default();
        self.drag = Some(Drag { vt, path: d.path, grab, panes });
        Press::Grabbed
    }

    /// The pointer moved to `p` with a divider grabbed: resize its split.
    /// Returns whether to redraw.
    pub fn drag(&mut self, tree: &mut Tree, frame: &Frame, p: (f32, f32)) -> bool {
        let Some(drag) = self.drag.as_ref() else {
            return false;
        };
        let Some(divider) = dragged_divider(tree, drag, frame) else {
            self.drag = None;
            return true;
        };
        let ratio = dragged_ratio(&divider, drag.grab, p, frame.cell);
        tree.set_ratio(drag.vt, &divider.path, ratio)
    }

    /// The left button went up: let go of the divider. Returns whether one was
    /// held.
    pub fn release(&mut self) -> bool {
        self.drag.take().is_some()
    }

    /// The divider to draw highlighted (hovered, or dragged: `true`).
    pub fn highlight(&self, tree: &Tree, frame: &Frame) -> Option<(Divider, bool)> {
        let (vt, divider, dragging) = match (&self.drag, &self.hover) {
            (Some(d), _) => (d.vt, dragged_divider(tree, d, frame)?, true),
            (None, Some((vt, path))) => {
                let d = tree.vtab(*vt)?.dividers(frame.workspace, frame.gap).into_iter().find(|d| &d.path == path)?;
                (*vt, d, false)
            }
            (None, None) => return None,
        };
        if tree.active_vtab() != Some(vt) {
            return None;
        }
        Some((divider, dragging))
    }
}

/// The divider of the active vtab a pointer at `p` grabs.
pub fn under(tree: &Tree, frame: &Frame, p: (f32, f32)) -> Option<(VtabId, Divider)> {
    let vt = tree.active_vtab()?;
    let d = tree.vtab(vt)?.divider_at(frame.workspace, frame.gap, frame.slop, p)?;
    Some((vt, d))
}

/// The vtab's panes in tree order.
fn pane_ids(v: &Vtab) -> Vec<PaneId> {
    v.panes().into_iter().map(|p| p.id).collect()
}

/// The divider `drag` holds, laid out in `frame`; none once its vtab closed or
/// its panes changed.
fn dragged_divider(tree: &Tree, drag: &Drag, frame: &Frame) -> Option<Divider> {
    let v = tree.vtab(drag.vt)?;
    if pane_ids(v) != drag.panes {
        return None;
    }
    v.dividers(frame.workspace, frame.gap).into_iter().find(|d| d.path == drag.path)
}

/// The least a pane may be dragged to along its split, in physical pixels.
fn pane_min(axis: Axis, cell_w: f32, cell_h: f32) -> f32 {
    match axis {
        Axis::LeftRight => MIN_PANE_COLS * cell_w,
        Axis::TopBottom => MIN_PANE_ROWS * cell_h,
    }
}

/// The ratio a drag with the pointer at `pointer` gives `divider`'s split.
fn dragged_ratio(divider: &Divider, grab: f32, pointer: (f32, f32), cell: (f32, f32)) -> f32 {
    divider.ratio_at(pointer, grab, pane_min(divider.axis, cell.0, cell.1))
}

#[cfg(test)]
mod tests {
    use super::{dragged_divider, dragged_ratio, pane_ids, pane_min, Drag, Frame};
    use ghostrealm_core::{Axis, Rect, Tree};

    #[test]
    fn dragging_a_divider_resizes_its_split_down_to_a_few_cells() {
        const GAP: f32 = 6.0;
        let ws = Rect { x: 200.0, y: 28.0, w: 806.0, h: 628.0 };
        let cell = (10.0, 20.0);
        let mut t = Tree::new();
        let (vt, left, _) = t.add_vtab("w");
        let (right, _) = t.split(vt, left, Axis::LeftRight).unwrap();
        let (bottom, _) = t.split(vt, right, Axis::TopBottom).unwrap();
        let rect_of = |t: &Tree, id| t.vtab(vt).unwrap().layout(ws, GAP).into_iter().find(|(p, _)| *p == id).unwrap().1;

        // Press on the vertical divider, 1 px off its centre; the grab holds that spot.
        let strip = rect_of(&t, left).x + rect_of(&t, left).w;
        let press = (strip + GAP * 0.5 + 1.0, 300.0);
        let d = t.vtab(vt).unwrap().divider_at(ws, GAP, 3.0, press).expect("grabbed");
        let grab = d.grab_offset(press);
        for (to, left_w) in [(400.0, 196.0), (250.0, 80.0), (9999.0, 800.0 - 80.0)] {
            let pointer = (to, 300.0);
            assert!(t.set_ratio(vt, &d.path, dragged_ratio(&d, grab, pointer, cell)));
            let w = rect_of(&t, left).w;
            assert!((w - left_w).abs() < 0.01, "pointer {to}: {w} != {left_w}");
            assert!(rect_of(&t, right).w >= 80.0 - 0.01, "the right pane keeps 8 columns");
        }
        // The stacked split was left alone.
        assert_eq!(rect_of(&t, right).h, rect_of(&t, bottom).h);
        // Double-click: back to halves.
        t.set_ratio(vt, &d.path, 0.5);
        assert_eq!(rect_of(&t, left).w, rect_of(&t, right).w);
    }

    #[test]
    fn a_drag_lets_go_when_a_pane_closes_under_it() {
        let frame = Frame { workspace: Rect { x: 0.0, y: 0.0, w: 806.0, h: 606.0 }, gap: 6.0, slop: 3.0, cell: (10.0, 20.0) };
        let mut t = Tree::new();
        let (vt, left, _) = t.add_vtab("w");
        let (right, _) = t.split(vt, left, Axis::LeftRight).unwrap();
        t.split(vt, right, Axis::TopBottom).unwrap();
        let drag = Drag { vt, path: vec![], grab: 0.0, panes: pane_ids(t.vtab(vt).unwrap()) };
        assert_eq!(dragged_divider(&t, &drag, &frame).unwrap().axis, Axis::LeftRight);
        t.close_pane(vt, left);
        let root = t.vtab(vt).unwrap().dividers(frame.workspace, frame.gap).remove(0);
        assert_eq!((root.path, root.axis), (vec![], Axis::TopBottom), "the path now names the other split");
        assert_eq!(dragged_divider(&t, &drag, &frame), None);
    }

    #[test]
    fn panes_keep_a_few_cells() {
        assert_eq!(pane_min(Axis::LeftRight, 9.0, 18.0), 72.0);
        assert_eq!(pane_min(Axis::TopBottom, 9.0, 18.0), 72.0);
    }
}
