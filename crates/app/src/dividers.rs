//! Split dividers under the mouse: the one the pointer is over, the one being
//! dragged, and a double-click to reset a split. Geometry and time are passed
//! in, so the press/drag/release flow is testable without a window.

use crate::clicks::{Click, ClickCounter};
use ghostrealm_core::{Divider, Pane, PaneId, Rect, SplitPath, Tree, Vtab, VtabId};

/// Dragging a divider leaves each pane's content (below its tab strip) at
/// least this many columns / rows.
const MIN_PANE_COLS: f32 = 8.0;
const MIN_PANE_ROWS: f32 = 4.0;

/// Where the dividers are laid out, and the sizes that set how small a drag
/// may make a pane. All in physical pixels.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub workspace: Rect,
    /// The gap between split children.
    pub gap: f32,
    /// How far either side of the gap a pointer still grabs it.
    pub slop: f32,
    pub cell: (f32, f32),
    /// A pane's tab strip, and whether a pane with at most one surface shows
    /// it (`[tabs] autohide_single_tab` off).
    pub strip_h: f32,
    pub strip_for_one: bool,
}

impl Frame {
    /// The least room pane `p` may be dragged to, (width, height): its tab
    /// strip, if shown, and a few cells of content.
    pub fn pane_min(&self, p: &Pane) -> (f32, f32) {
        let strip = if p.surfaces.len() > 1 || self.strip_for_one { self.strip_h } else { 0.0 };
        (MIN_PANE_COLS * self.cell.0, MIN_PANE_ROWS * self.cell.1 + strip)
    }

    /// Resize the split at `path` of `vt` to `ratio`, keeping every pane in it
    /// at least [`Self::pane_min`].
    fn resize(&self, tree: &mut Tree, vt: VtabId, path: &[bool], ratio: f32) -> bool {
        tree.resize_split(vt, path, ratio, self.workspace, self.gap, &|p| self.pane_min(p))
    }
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
    /// Presses on dividers, counted apart from other presses: a click in a
    /// pane next to a divider is not half of a double-click on it.
    clicks: ClickCounter,
    /// The divider the counted presses landed on. Where grab areas meet, a
    /// press at the same spot can land on another, which starts a new count.
    clicked: Option<(VtabId, SplitPath)>,
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

    /// A left press, `click`, on divider `d` of `vt`: grab it to drag, or reset
    /// its split to halves on a double-click.
    pub fn press(&mut self, tree: &mut Tree, frame: &Frame, (vt, d): (VtabId, Divider), click: &Click) -> Press {
        let hit = Some((vt, d.path.clone()));
        if self.clicked != hit {
            self.clicks.reset();
            self.clicked = hit;
        }
        if self.clicks.count(click) >= 2 {
            frame.resize(tree, vt, &d.path, 0.5);
            return Press::Reset;
        }
        let grab = d.grab_offset(click.pos);
        let panes = tree.vtab(vt).map(pane_ids).unwrap_or_default();
        self.drag = Some(Drag { vt, path: d.path, grab, panes });
        Press::Grabbed
    }

    /// A left press that missed the dividers: the next press on one starts a
    /// new multi-click.
    pub fn missed(&mut self) {
        self.clicks.reset();
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
        frame.resize(tree, drag.vt, &divider.path, divider.ratio_at(p, drag.grab))
    }

    /// The left button went up: let go of the divider. Returns whether one was
    /// held.
    pub fn release(&mut self) -> bool {
        self.drag.take().is_some()
    }

    /// The divider to draw highlighted (hovered, or dragged: `true`).
    pub fn highlight(&self, tree: &Tree, frame: &Frame) -> Option<(Divider, bool)> {
        if let Some(drag) = &self.drag {
            return dragged_divider(tree, drag, frame).map(|d| (d, true));
        }
        let (vt, path) = self.hover.as_ref()?;
        if tree.active_vtab() != Some(*vt) {
            return None;
        }
        let d = tree.vtab(*vt)?.dividers(frame.workspace, frame.gap).into_iter().find(|d| &d.path == path)?;
        Some((d, false))
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

/// The divider `drag` holds, laid out in `frame`; none once its vtab closed,
/// went out of view, or its panes changed.
fn dragged_divider(tree: &Tree, drag: &Drag, frame: &Frame) -> Option<Divider> {
    if tree.active_vtab() != Some(drag.vt) {
        return None;
    }
    let v = tree.vtab(drag.vt)?;
    if pane_ids(v) != drag.panes {
        return None;
    }
    v.dividers(frame.workspace, frame.gap).into_iter().find(|d| d.path == drag.path)
}

#[cfg(test)]
mod tests {
    use super::{dragged_divider, pane_ids, under, Dividers, Drag, Frame, Press};
    use crate::clicks::Click;
    use ghostrealm_core::{Axis, PaneId, Rect, Tree, VtabId};
    use std::time::{Duration, Instant};

    /// 806x606: 800 px for two side-by-side panes, the strip at x = 400..406.
    const FRAME: Frame = Frame {
        workspace: Rect { x: 0.0, y: 0.0, w: 806.0, h: 606.0 },
        gap: 6.0,
        slop: 3.0,
        cell: (10.0, 20.0),
        strip_h: 26.0,
        strip_for_one: false,
    };

    fn side_by_side() -> (Tree, VtabId, PaneId) {
        let mut t = Tree::new();
        let (vt, left, _) = t.add_vtab("w");
        t.split(vt, left, Axis::LeftRight).unwrap();
        (t, vt, left)
    }

    fn width(t: &Tree, vt: VtabId, pane: PaneId) -> f32 {
        t.vtab(vt).unwrap().layout(FRAME.workspace, FRAME.gap).into_iter().find(|(p, _)| *p == pane).unwrap().1.w
    }

    /// A press at `pos`, `ms` after the first, with a 400 ms double-click.
    fn click(t0: Instant, ms: u64, pos: (f32, f32)) -> Click {
        Click {
            button: crate::plugin::MouseButton::Left,
            at: t0 + Duration::from_millis(ms),
            pos,
            window: Duration::from_millis(400),
            slop: 4.0,
        }
    }

    #[test]
    fn a_press_on_a_divider_drags_it_until_the_release() {
        let (mut t, vt, left) = side_by_side();
        let mut d = Dividers::default();
        let t0 = Instant::now();
        let at = (403.0, 300.0);
        let hit = under(&t, &FRAME, at).expect("on the divider");
        assert!(d.hover(Some(&hit)));
        assert_eq!(d.press(&mut t, &FRAME, hit, &click(t0, 0, at)), Press::Grabbed);
        assert!(d.drag(&mut t, &FRAME, (203.0, 300.0)));
        assert_eq!(width(&t, vt, left), 200.0, "the grabbed spot stays under the pointer");
        let (lit, dragging) = d.highlight(&t, &FRAME).expect("lit while dragged");
        assert_eq!((lit.strip.x, dragging), (200.0, true));
        assert!(d.release());
        assert!(!d.drag(&mut t, &FRAME, (603.0, 300.0)), "let go");
        assert_eq!(width(&t, vt, left), 200.0);
        assert!(!d.release(), "nothing held");
    }

    #[test]
    fn a_drag_whose_release_was_lost_lets_go_at_the_next_press() {
        let (mut t, vt, left) = side_by_side();
        let mut d = Dividers::default();
        let at = (403.0, 300.0);
        let hit = under(&t, &FRAME, at).unwrap();
        assert_eq!(d.press(&mut t, &FRAME, hit, &click(Instant::now(), 0, at)), Press::Grabbed);
        assert!(d.drag(&mut t, &FRAME, (303.0, 300.0)));
        // No release; the next press, in a pane, lets go first (the window's
        // `release_lost_press`).
        assert!(d.release());
        d.missed();
        assert!(!d.drag(&mut t, &FRAME, (603.0, 300.0)), "moves no longer resize");
        assert_eq!(width(&t, vt, left), 300.0);
    }

    #[test]
    fn a_double_click_on_a_divider_resets_its_split_to_halves() {
        let (mut t, vt, left) = side_by_side();
        FRAME.resize(&mut t, vt, &[], 0.25);
        let mut d = Dividers::default();
        let t0 = Instant::now();
        let at = (203.0, 300.0);
        let hit = under(&t, &FRAME, at).expect("on the divider");
        assert_eq!(d.press(&mut t, &FRAME, hit, &click(t0, 0, at)), Press::Grabbed);
        assert!(d.release());
        let hit = under(&t, &FRAME, at).unwrap();
        assert_eq!(d.press(&mut t, &FRAME, hit, &click(t0, 150, at)), Press::Reset);
        assert_eq!(width(&t, vt, left), 400.0);
        assert!(!d.dragging(), "a reset grabs nothing");
    }

    #[test]
    fn a_double_click_across_two_dividers_resets_neither() {
        // left | (top / bottom): at the T-junction a press lands on either.
        let (mut t, vt, left) = side_by_side();
        let right = t.vtab(vt).unwrap().panes()[1].id;
        t.split(vt, right, Axis::TopBottom).unwrap();
        FRAME.resize(&mut t, vt, &[], 0.25);
        FRAME.resize(&mut t, vt, &[true], 0.25);
        let mut d = Dividers::default();
        let t0 = Instant::now();
        let at = (206.0, 151.0);
        let root = t.vtab(vt).unwrap().dividers(FRAME.workspace, FRAME.gap).remove(0);
        let nested = under(&t, &FRAME, at).expect("on the nested divider");
        assert_eq!(nested.1.path, vec![true]);
        assert_eq!(d.press(&mut t, &FRAME, (vt, root), &click(t0, 0, at)), Press::Grabbed);
        assert!(d.release());
        assert_eq!(d.press(&mut t, &FRAME, nested, &click(t0, 100, at)), Press::Grabbed);
        assert!(d.release());
        assert_eq!(width(&t, vt, left), 200.0, "the root split kept its ratio");
        let top = t.vtab(vt).unwrap().layout(FRAME.workspace, FRAME.gap)[1].1.h;
        assert_eq!(top, 150.0, "and so did the nested one");
    }

    #[test]
    fn a_press_elsewhere_between_two_on_a_divider_is_no_double_click() {
        let (mut t, vt, left) = side_by_side();
        FRAME.resize(&mut t, vt, &[], 0.25);
        let mut d = Dividers::default();
        let t0 = Instant::now();
        let at = (203.0, 300.0);
        // A pane click beside the divider, then a press on it.
        d.missed();
        let hit = under(&t, &FRAME, at).unwrap();
        assert_eq!(d.press(&mut t, &FRAME, hit, &click(t0, 100, at)), Press::Grabbed);
        assert!(d.release());
        // A press on it, a pane click, and another press on it.
        d.missed();
        let hit = under(&t, &FRAME, at).unwrap();
        assert_eq!(d.press(&mut t, &FRAME, hit, &click(t0, 200, at)), Press::Grabbed);
        assert_eq!(width(&t, vt, left), 200.0, "the split kept its ratio");
    }

    #[test]
    fn a_drag_leaves_each_pane_a_few_cells_of_content() {
        // left | (top / bottom), in 806x628 at (200, 28).
        let frame = Frame { workspace: Rect { x: 200.0, y: 28.0, w: 806.0, h: 628.0 }, ..FRAME };
        let mut t = Tree::new();
        let (vt, left, _) = t.add_vtab("w");
        let (top, _) = t.split(vt, left, Axis::LeftRight).unwrap();
        let (bottom, _) = t.split(vt, top, Axis::TopBottom).unwrap();
        let rect_of = |t: &Tree, id| t.vtab(vt).unwrap().layout(frame.workspace, frame.gap).into_iter().find(|(p, _)| *p == id).unwrap().1;
        let near = |a: f32, b: f32| (a - b).abs() < 0.01;
        let mut d = Dividers::default();
        let t0 = Instant::now();

        // Grab the vertical divider 1 px off its centre; the grab holds that spot.
        let press = (rect_of(&t, left).x + rect_of(&t, left).w + 4.0, 300.0);
        let hit = under(&t, &frame, press).expect("grabbed");
        assert_eq!(d.press(&mut t, &frame, hit, &click(t0, 0, press)), Press::Grabbed);
        for (to, left_w) in [(400.0, 196.0), (250.0, 80.0), (9999.0, 800.0 - 80.0)] {
            assert!(d.drag(&mut t, &frame, (to, 300.0)));
            let w = rect_of(&t, left).w;
            assert!(near(w, left_w), "pointer {to}: {w} != {left_w}");
        }
        assert!(near(rect_of(&t, top).w, 80.0), "the right panes keep 8 columns");
        assert!(d.release());
        assert_eq!(rect_of(&t, top).h, rect_of(&t, bottom).h, "the stacked split was left alone");

        // The stacked divider: the top pane keeps 4 rows.
        let strip_y = |t: &Tree| rect_of(t, top).y + rect_of(t, top).h;
        let press = (960.0, strip_y(&t) + 3.0);
        let hit = under(&t, &frame, press).expect("grabbed");
        assert_eq!(d.press(&mut t, &frame, hit, &click(t0, 1000, press)), Press::Grabbed);
        assert!(d.drag(&mut t, &frame, (960.0, -1000.0)));
        assert!(near(rect_of(&t, top).h, 80.0));
        assert!(d.release());
        // With a tab strip shown, 4 rows of content below it.
        t.add_surface(vt, top);
        let press = (960.0, strip_y(&t) + 3.0);
        let hit = under(&t, &frame, press).expect("grabbed");
        assert_eq!(d.press(&mut t, &frame, hit, &click(t0, 2000, press)), Press::Grabbed);
        assert!(d.drag(&mut t, &frame, (960.0, -1000.0)));
        assert!(near(rect_of(&t, top).h, 80.0 + 26.0));
    }

    #[test]
    fn a_drag_lets_go_when_a_pane_closes_under_it() {
        let frame = FRAME;
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
    fn a_drag_lets_go_when_its_vtab_goes_out_of_view() {
        let (mut t, vt, left) = side_by_side();
        let mut d = Dividers::default();
        let at = (403.0, 300.0);
        let hit = under(&t, &FRAME, at).unwrap();
        assert_eq!(d.press(&mut t, &FRAME, hit, &click(Instant::now(), 0, at)), Press::Grabbed);
        t.add_vtab("other");
        assert!(d.drag(&mut t, &FRAME, (203.0, 300.0)));
        assert!(!d.dragging() && d.highlight(&t, &FRAME).is_none());
        t.focus_vtab(vt);
        assert!(!d.drag(&mut t, &FRAME, (203.0, 300.0)), "not picked up again");
        assert_eq!(width(&t, vt, left), 400.0);
    }

    #[test]
    fn the_hover_follows_the_layout_under_the_pointer() {
        let (mut t, vt, left) = side_by_side();
        let mut d = Dividers::default();
        let at = (403.0, 300.0);
        assert!(d.hover(under(&t, &FRAME, at).as_ref()));
        assert_eq!(d.highlight(&t, &FRAME).map(|(h, dragging)| (h.strip.x, dragging)), Some((400.0, false)));
        // The split moves away from the pointer: refreshed, nothing is lit.
        FRAME.resize(&mut t, vt, &[], 0.25);
        assert!(d.hover(under(&t, &FRAME, at).as_ref()));
        assert_eq!(d.highlight(&t, &FRAME), None);
        // Split again under the pointer: the new divider is the one lit.
        let right = t.vtab(vt).unwrap().panes()[1].id;
        t.split(vt, right, Axis::TopBottom).unwrap();
        FRAME.resize(&mut t, vt, &[], 0.5);
        let at = (600.0, 303.0);
        assert!(d.hover(under(&t, &FRAME, at).as_ref()));
        assert_eq!(d.highlight(&t, &FRAME).map(|(h, _)| (h.path, h.axis)), Some((vec![true], Axis::TopBottom)));
        // A drag let go away from its divider: the hover is the pointer's again.
        let at = (403.0, 100.0);
        let hit = under(&t, &FRAME, at).unwrap();
        assert!(d.press(&mut t, &FRAME, hit, &click(Instant::now(), 0, at)) == Press::Grabbed);
        assert!(d.drag(&mut t, &FRAME, (-500.0, 100.0)));
        assert!(d.release());
        assert!(d.hover(under(&t, &FRAME, (-500.0, 100.0)).as_ref()));
        assert_eq!(d.highlight(&t, &FRAME), None);
        assert!(width(&t, vt, left) < 400.0);
    }

    #[test]
    fn a_pane_keeps_a_few_cells_below_its_tab_strip() {
        let mut t = Tree::new();
        let (vt, pane, _) = t.add_vtab("w");
        let min = |t: &Tree, frame: &Frame| frame.pane_min(t.vtab(vt).unwrap().panes()[0]);
        assert_eq!(min(&t, &FRAME), (80.0, 80.0), "one surface: no strip");
        assert_eq!(min(&t, &Frame { strip_for_one: true, ..FRAME }), (80.0, 106.0));
        t.add_surface(vt, pane);
        assert_eq!(min(&t, &FRAME), (80.0, 106.0));
    }
}
