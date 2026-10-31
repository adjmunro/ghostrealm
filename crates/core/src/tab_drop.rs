//! Dropping a dragged tab (surface) in its workspace: the zone under the
//! pointer, the split tree that results, and the rect of the pane the tab
//! lands in, which the drag previews.

use crate::tree::{Axis, Node, Pane, PaneId, Rect, SurfaceId, Tree, Vtab, VtabId};

/// A side of a pane or of the workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    const ALL: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

    fn axis(self) -> Axis {
        match self {
            Edge::Left | Edge::Right => Axis::LeftRight,
            Edge::Top | Edge::Bottom => Axis::TopBottom,
        }
    }

    /// The strip of `r` along this side, `depth` deep.
    fn strip(self, r: Rect, depth: f32) -> Rect {
        match self {
            Edge::Left => Rect { w: depth, ..r },
            Edge::Right => Rect { x: r.x + r.w - depth, w: depth, ..r },
            Edge::Top => Rect { h: depth, ..r },
            Edge::Bottom => Rect { y: r.y + r.h - depth, h: depth, ..r },
        }
    }

    /// How far `p` is inside `r` from this side.
    fn depth(self, r: Rect, p: (f32, f32)) -> f32 {
        match self {
            Edge::Left => p.0 - r.x,
            Edge::Right => r.x + r.w - p.0,
            Edge::Top => p.1 - r.y,
            Edge::Bottom => r.y + r.h - p.1,
        }
    }
}

/// Where a dragged tab drops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabDrop {
    /// Into the pane's tab strip, as its last tab.
    Into(PaneId),
    /// Into a new pane on that side of the pane, each taking half its room.
    Split(PaneId, Edge),
    /// Into a new pane on that side of the whole workspace, each taking half.
    Workspace(Edge),
}

/// A pane's centre zone: the middle half of it each way. Outside it, the
/// side the pointer is furthest towards, relative to the pane's size, wins.
const CENTRE: f32 = 0.25;

impl Vtab {
    /// The drop zone at `p` with the vtab laid out in `rect`: within `band`
    /// of the workspace's edge, that edge (the nearest at a corner); else the
    /// centre or a side of the pane under `p`. `None` off the workspace and
    /// on dividers.
    pub fn tab_drop_at(&self, rect: Rect, gap: f32, band: f32, p: (f32, f32)) -> Option<TabDrop> {
        if !contains(rect, p) {
            return None;
        }
        let near = Edge::ALL
            .into_iter()
            .map(|e| (e, e.depth(rect, p)))
            .filter(|&(_, d)| d < band)
            .min_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((e, _)) = near {
            return Some(TabDrop::Workspace(e));
        }
        let (pane, r) = self.layout(rect, gap).into_iter().find(|&(_, r)| contains(r, p))?;
        let dx = (p.0 - (r.x + r.w / 2.0)) / r.w;
        let dy = (p.1 - (r.y + r.h / 2.0)) / r.h;
        if dx.abs() < CENTRE && dy.abs() < CENTRE {
            return Some(TabDrop::Into(pane));
        }
        let edge = match (dx.abs() >= dy.abs(), dx < 0.0, dy < 0.0) {
            (true, true, _) => Edge::Left,
            (true, false, _) => Edge::Right,
            (false, _, true) => Edge::Top,
            (false, _, false) => Edge::Bottom,
        };
        Some(TabDrop::Split(pane, edge))
    }

    /// The rect the pane holding surface `sid` would have after dropping it
    /// at `to`, laid out in `rect`. `None` when the drop changes nothing.
    pub fn tab_drop_preview(&self, rect: Rect, gap: f32, sid: SurfaceId, to: TabDrop) -> Option<Rect> {
        let (root, lands) = dropped(&self.root, sid, to, PaneId(0))?;
        root.layout(rect, gap).into_iter().find(|&(id, _)| id == lands).map(|(_, r)| r)
    }

    /// Every zone a drag of surface `sid` can drop in, with the part of the
    /// workspace it covers: the workspace's edge bands, and each pane's
    /// centre and the middle of each of its sides (inside the bands). Zones
    /// whose drop would change nothing are left out.
    pub fn tab_drop_zones(&self, rect: Rect, gap: f32, band: f32, sid: SurfaceId) -> Vec<(TabDrop, Rect)> {
        let inner = Rect { x: rect.x + band, y: rect.y + band, w: rect.w - 2.0 * band, h: rect.h - 2.0 * band };
        let mut zones: Vec<(TabDrop, Rect)> =
            Edge::ALL.into_iter().map(|e| (TabDrop::Workspace(e), e.strip(rect, band))).collect();
        for (pane, r) in self.layout(rect, gap) {
            let (qw, qh) = (r.w * CENTRE, r.h * CENTRE);
            let centre = Rect { x: r.x + qw, y: r.y + qh, w: r.w / 2.0, h: r.h / 2.0 };
            zones.push((TabDrop::Into(pane), centre));
            for e in Edge::ALL {
                let tile = match e.axis() {
                    Axis::LeftRight => Rect { y: centre.y, h: centre.h, ..e.strip(r, qw) },
                    Axis::TopBottom => Rect { x: centre.x, w: centre.w, ..e.strip(r, qh) },
                };
                zones.push((TabDrop::Split(pane, e), tile));
            }
        }
        zones
            .into_iter()
            .filter_map(|(to, r)| {
                let r = if matches!(to, TabDrop::Workspace(_)) { r } else { intersect(r, inner)? };
                self.tab_drop_preview(rect, gap, sid, to).map(|_| (to, r))
            })
            .collect()
    }
}

impl Tree {
    /// Move surface `sid` of `vtab` to `to`; the pane it lands in becomes
    /// focused. A pane left with no surfaces closes. Returns false when the
    /// drop would change nothing or names nothing in the vtab.
    pub fn move_surface(&mut self, vtab: VtabId, sid: SurfaceId, to: TabDrop) -> bool {
        let new_id = PaneId(self.fresh());
        let Some(v) = self.vtab_mut(vtab) else {
            return false;
        };
        let Some((root, lands)) = dropped(&v.root, sid, to, new_id) else {
            return false;
        };
        v.root = root;
        v.focused_pane = lands;
        true
    }
}

/// `root` after moving surface `sid` to `to`, a new pane taking id `new_id`,
/// and the pane the surface lands in. `None` when the layout would stay as it
/// is (into its own pane, or its pane's only tab onto that pane or the sole
/// pane's edges) or `sid` or the target pane isn't in `root`.
fn dropped(root: &Node, sid: SurfaceId, to: TabDrop, new_id: PaneId) -> Option<(Node, PaneId)> {
    let mut root = root.clone();
    let from = pane_holding(&root, sid)?;
    let target = match to {
        TabDrop::Into(t) | TabDrop::Split(t, _) => Some(t),
        TabDrop::Workspace(_) => None,
    };
    if let Some(t) = target {
        root.find_pane_mut(t)?;
    }
    let src = root.find_pane_mut(from)?;
    let alone = src.surfaces.len() == 1;
    let onto_self = target == Some(from) && (alone || matches!(to, TabDrop::Into(_)));
    if onto_self || (alone && matches!(root, Node::Leaf(_))) {
        return None;
    }
    let surface = root.find_pane_mut(from)?.remove_surface(sid)?;
    if alone {
        root = root.remove_pane(from)?;
    }
    let new = Pane { id: new_id, surfaces: vec![surface], active: 0 };
    let lands = match to {
        TabDrop::Into(t) => {
            let p = root.find_pane_mut(t)?;
            p.surfaces.extend(new.surfaces);
            p.active = p.surfaces.len() - 1;
            t
        }
        TabDrop::Split(t, e) => {
            wrap_leaf(&mut root, t, e, &mut Some(new));
            new_id
        }
        TabDrop::Workspace(e) => {
            root = wrap(root, e, Node::Leaf(new));
            new_id
        }
    };
    Some((root, lands))
}

/// A half-and-half split of `node` and `new`, with `new` on side `e`.
fn wrap(node: Node, e: Edge, new: Node) -> Node {
    let (first, second) = if matches!(e, Edge::Left | Edge::Top) { (new, node) } else { (node, new) };
    Node::Split { axis: e.axis(), ratio: 0.5, first: Box::new(first), second: Box::new(second) }
}

/// Replace leaf `target` under `node` with [`wrap`] of it and `new`.
fn wrap_leaf(node: &mut Node, target: PaneId, e: Edge, new: &mut Option<Pane>) {
    match node {
        Node::Leaf(p) if p.id == target => {
            let Some(new) = new.take() else { return };
            let old = std::mem::replace(node, Node::Leaf(Pane { id: PaneId(0), surfaces: Vec::new(), active: 0 }));
            *node = wrap(old, e, Node::Leaf(new));
        }
        Node::Leaf(_) => {}
        Node::Split { first, second, .. } => {
            wrap_leaf(first, target, e, new);
            wrap_leaf(second, target, e, new);
        }
    }
}

fn pane_holding(node: &Node, sid: SurfaceId) -> Option<PaneId> {
    match node {
        Node::Leaf(p) => p.surfaces.iter().any(|s| s.id == sid).then_some(p.id),
        Node::Split { first, second, .. } => pane_holding(first, sid).or_else(|| pane_holding(second, sid)),
    }
}

fn contains(r: Rect, p: (f32, f32)) -> bool {
    p.0 >= r.x && p.0 < r.x + r.w && p.1 >= r.y && p.1 < r.y + r.h
}

fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let (x, y) = (a.x.max(b.x), a.y.max(b.y));
    let (w, h) = ((a.x + a.w).min(b.x + b.w) - x, (a.y + a.h).min(b.y + b.h) - y);
    (w > 0.0 && h > 0.0).then_some(Rect { x, y, w, h })
}

#[cfg(test)]
mod tests {
    use super::*;

    const WS: Rect = Rect { x: 0.0, y: 0.0, w: 1000.0, h: 600.0 };
    const GAP: f32 = 0.0;
    const BAND: f32 = 10.0;

    /// A workspace of two panes side by side: `left` with two tabs, `right`
    /// with one. Returns the tree, the vtab, the panes and the left tabs.
    fn side_by_side() -> (Tree, VtabId, PaneId, PaneId, SurfaceId, SurfaceId) {
        let mut t = Tree::new();
        let (vt, left, s1) = t.add_vtab("w");
        let s2 = t.add_surface(vt, left).unwrap();
        let (right, _) = t.split(vt, left, Axis::LeftRight).unwrap();
        (t, vt, left, right, s1, s2)
    }

    fn vtab(t: &Tree, vt: VtabId) -> &Vtab {
        t.vtab(vt).unwrap()
    }

    fn rect_of(t: &Tree, vt: VtabId, pane: PaneId) -> Rect {
        vtab(t, vt).layout(WS, GAP).into_iter().find(|&(id, _)| id == pane).unwrap().1
    }

    fn pane_of(t: &Tree, vt: VtabId, sid: SurfaceId) -> PaneId {
        pane_holding(&vtab(t, vt).root, sid).unwrap()
    }

    fn surfaces(t: &Tree, vt: VtabId, pane: PaneId) -> Vec<SurfaceId> {
        vtab(t, vt).panes().into_iter().find(|p| p.id == pane).unwrap().surfaces.iter().map(|s| s.id).collect()
    }

    #[test]
    fn the_workspace_edge_bands_win_and_the_nearer_edge_takes_a_corner() {
        let (t, vt, ..) = side_by_side();
        let at = |p| vtab(&t, vt).tab_drop_at(WS, GAP, BAND, p);
        assert_eq!(at((300.0, 595.0)), Some(TabDrop::Workspace(Edge::Bottom)));
        assert_eq!(at((300.0, 2.0)), Some(TabDrop::Workspace(Edge::Top)));
        assert_eq!(at((1.0, 300.0)), Some(TabDrop::Workspace(Edge::Left)));
        assert_eq!(at((999.0, 300.0)), Some(TabDrop::Workspace(Edge::Right)));
        assert_eq!(at((2.0, 596.0)), Some(TabDrop::Workspace(Edge::Left)));
        assert_eq!(at((5.0, 598.0)), Some(TabDrop::Workspace(Edge::Bottom)));
        assert_eq!(at((300.0, 600.0)), None, "off the workspace");
    }

    #[test]
    fn inside_the_bands_a_panes_centre_and_sides_are_its_zones() {
        let (t, vt, left, right, ..) = side_by_side();
        let at = |p| vtab(&t, vt).tab_drop_at(WS, GAP, BAND, p);
        // `left` is (0, 0, 500, 600): its centre is (125..375, 150..450).
        assert_eq!(at((250.0, 300.0)), Some(TabDrop::Into(left)));
        assert_eq!(at((370.0, 440.0)), Some(TabDrop::Into(left)));
        assert_eq!(at((20.0, 300.0)), Some(TabDrop::Split(left, Edge::Left)));
        assert_eq!(at((480.0, 300.0)), Some(TabDrop::Split(left, Edge::Right)));
        assert_eq!(at((250.0, 20.0)), Some(TabDrop::Split(left, Edge::Top)));
        assert_eq!(at((250.0, 580.0)), Some(TabDrop::Split(left, Edge::Bottom)));
        // Towards a corner the side it is further towards, for the pane's size, wins.
        assert_eq!(at((60.0, 100.0)), Some(TabDrop::Split(left, Edge::Left)));
        assert_eq!(at((100.0, 40.0)), Some(TabDrop::Split(left, Edge::Top)));
        assert_eq!(at((520.0, 300.0)), Some(TabDrop::Split(right, Edge::Left)));
        assert_eq!(at((750.0, 300.0)), Some(TabDrop::Into(right)));
    }

    #[test]
    fn a_divider_is_no_zone() {
        let (t, vt, ..) = side_by_side();
        let v = vtab(&t, vt);
        // With a 6 px gap the divider strip is 497..503.
        assert_eq!(v.tab_drop_at(WS, 6.0, BAND, (500.0, 300.0)), None);
    }

    #[test]
    fn a_side_zone_splits_that_pane_with_the_tab_on_that_side() {
        let (mut t, vt, left, right, s1, s2) = side_by_side();
        assert!(t.move_surface(vt, s2, TabDrop::Split(right, Edge::Bottom)));
        let new = pane_of(&t, vt, s2);
        assert_eq!(t.vtab(vt).unwrap().focused_pane, new);
        assert_eq!(surfaces(&t, vt, left), vec![s1]);
        assert_eq!(rect_of(&t, vt, left), Rect { x: 0.0, y: 0.0, w: 500.0, h: 600.0 });
        assert_eq!(rect_of(&t, vt, right), Rect { x: 500.0, y: 0.0, w: 500.0, h: 300.0 });
        assert_eq!(rect_of(&t, vt, new), Rect { x: 500.0, y: 300.0, w: 500.0, h: 300.0 });

        let (mut t, vt, left, right, _, s2) = side_by_side();
        assert!(t.move_surface(vt, s2, TabDrop::Split(left, Edge::Left)));
        let new = pane_of(&t, vt, s2);
        assert_eq!(rect_of(&t, vt, new), Rect { x: 0.0, y: 0.0, w: 250.0, h: 600.0 });
        assert_eq!(rect_of(&t, vt, left), Rect { x: 250.0, y: 0.0, w: 250.0, h: 600.0 });
        assert_eq!(rect_of(&t, vt, right).x, 500.0);
    }

    #[test]
    fn the_centre_moves_the_tab_into_that_panes_strip_as_its_active_tab() {
        let (mut t, vt, left, right, s1, s2) = side_by_side();
        let s3 = surfaces(&t, vt, right)[0];
        assert!(t.move_surface(vt, s1, TabDrop::Into(right)));
        assert_eq!(surfaces(&t, vt, left), vec![s2]);
        assert_eq!(surfaces(&t, vt, right), vec![s3, s1]);
        let v = t.vtab(vt).unwrap();
        assert_eq!(v.focused_pane, right);
        assert_eq!(v.panes()[1].active, 1);
    }

    #[test]
    fn the_bottom_band_makes_a_full_width_pane_under_a_side_by_side_split() {
        let (mut t, vt, left, right, _, s2) = side_by_side();
        assert!(t.move_surface(vt, s2, TabDrop::Workspace(Edge::Bottom)));
        let new = pane_of(&t, vt, s2);
        assert_eq!(rect_of(&t, vt, new), Rect { x: 0.0, y: 300.0, w: 1000.0, h: 300.0 });
        assert_eq!(rect_of(&t, vt, left), Rect { x: 0.0, y: 0.0, w: 500.0, h: 300.0 });
        assert_eq!(rect_of(&t, vt, right), Rect { x: 500.0, y: 0.0, w: 500.0, h: 300.0 });
        let Node::Split { axis: Axis::TopBottom, first, second, .. } = &t.vtab(vt).unwrap().root else {
            panic!("the root splits top and bottom");
        };
        assert!(matches!(**first, Node::Split { axis: Axis::LeftRight, .. }));
        assert!(matches!(**second, Node::Leaf(Pane { id, .. }) if id == new));
    }

    #[test]
    fn each_workspace_band_puts_the_new_pane_on_its_side() {
        for (e, want) in [
            (Edge::Left, Rect { x: 0.0, y: 0.0, w: 500.0, h: 600.0 }),
            (Edge::Right, Rect { x: 500.0, y: 0.0, w: 500.0, h: 600.0 }),
            (Edge::Top, Rect { x: 0.0, y: 0.0, w: 1000.0, h: 300.0 }),
        ] {
            let (mut t, vt, .., s2) = side_by_side();
            assert!(t.move_surface(vt, s2, TabDrop::Workspace(e)));
            assert_eq!(rect_of(&t, vt, pane_of(&t, vt, s2)), want, "{e:?}");
        }
    }

    #[test]
    fn a_panes_only_tab_moving_out_closes_the_pane() {
        let (mut t, vt, left, right, s1, s2) = side_by_side();
        let s3 = surfaces(&t, vt, right)[0];
        assert!(t.move_surface(vt, s3, TabDrop::Workspace(Edge::Bottom)));
        let v = t.vtab(vt).unwrap();
        assert_eq!(v.panes().len(), 2);
        assert!(!v.panes().iter().any(|p| p.id == right));
        assert_eq!(rect_of(&t, vt, left), Rect { x: 0.0, y: 0.0, w: 1000.0, h: 300.0 });
        assert_eq!(surfaces(&t, vt, left), vec![s1, s2]);
    }

    #[test]
    fn drops_that_change_nothing_are_refused() {
        let (mut t, vt, left, right, s1, _) = side_by_side();
        let s3 = surfaces(&t, vt, right)[0];
        let before = t.clone();
        assert!(!t.move_surface(vt, s1, TabDrop::Into(left)));
        assert!(!t.move_surface(vt, s3, TabDrop::Into(right)));
        assert!(!t.move_surface(vt, s3, TabDrop::Split(right, Edge::Top)));
        assert!(!t.move_surface(vt, SurfaceId(999), TabDrop::Workspace(Edge::Top)));
        assert!(!t.move_surface(vt, s1, TabDrop::Into(PaneId(999))));
        assert_eq!(t.vtabs(), before.vtabs());

        let mut t = Tree::new();
        let (vt, _, sole) = t.add_vtab("w");
        assert!(!t.move_surface(vt, sole, TabDrop::Workspace(Edge::Bottom)));
    }

    #[test]
    fn a_tab_can_split_its_own_pane_when_the_pane_keeps_another() {
        let (mut t, vt, left, _, s1, s2) = side_by_side();
        assert!(t.move_surface(vt, s2, TabDrop::Split(left, Edge::Top)));
        let new = pane_of(&t, vt, s2);
        assert_eq!(rect_of(&t, vt, new), Rect { x: 0.0, y: 0.0, w: 500.0, h: 300.0 });
        assert_eq!(rect_of(&t, vt, left), Rect { x: 0.0, y: 300.0, w: 500.0, h: 300.0 });
        assert_eq!(surfaces(&t, vt, left), vec![s1]);
    }

    #[test]
    fn the_preview_is_the_rect_of_the_pane_the_tab_lands_in() {
        let (t, vt, left, right, s1, s2) = side_by_side();
        let s3 = surfaces(&t, vt, right)[0];
        let mut drops = vec![
            (s2, TabDrop::Into(right)),
            (s2, TabDrop::Workspace(Edge::Bottom)),
            (s3, TabDrop::Workspace(Edge::Bottom)),
            (s1, TabDrop::Split(left, Edge::Top)),
        ];
        for e in Edge::ALL {
            drops.push((s2, TabDrop::Split(right, e)));
            drops.push((s2, TabDrop::Workspace(e)));
        }
        for gap in [0.0, 6.0] {
            for &(sid, to) in &drops {
                let preview = vtab(&t, vt).tab_drop_preview(WS, gap, sid, to).unwrap();
                let mut after = t.clone();
                assert!(after.move_surface(vt, sid, to));
                let lands = pane_of(&after, vt, sid);
                let got = vtab(&after, vt).layout(WS, gap).into_iter().find(|&(id, _)| id == lands).unwrap().1;
                assert_eq!(preview, got, "{to:?} gap {gap}");
            }
        }
        assert_eq!(vtab(&t, vt).tab_drop_preview(WS, GAP, s3, TabDrop::Into(right)), None);
    }

    #[test]
    fn zones_cover_each_pane_and_band_and_skip_drops_that_change_nothing() {
        let (t, vt, left, right, s1, ..) = side_by_side();
        let s3 = surfaces(&t, vt, right)[0];
        let zones = vtab(&t, vt).tab_drop_zones(WS, GAP, BAND, s3);
        let has = |to| zones.iter().any(|(z, _)| *z == to);
        for e in Edge::ALL {
            assert!(has(TabDrop::Workspace(e)));
            assert!(has(TabDrop::Split(left, e)));
            assert!(!has(TabDrop::Split(right, e)), "the only tab onto its own pane");
        }
        assert!(has(TabDrop::Into(left)));
        assert!(!has(TabDrop::Into(right)));
        // Each zone's rect hit-tests back to that zone.
        for &(to, r) in &zones {
            let mid = (r.x + r.w / 2.0, r.y + r.h / 2.0);
            assert_eq!(vtab(&t, vt).tab_drop_at(WS, GAP, BAND, mid), Some(to), "{to:?} at {r:?}");
        }
        let own = vtab(&t, vt).tab_drop_zones(WS, GAP, BAND, s1);
        assert!(own.iter().any(|(z, _)| *z == TabDrop::Split(left, Edge::Top)));
        assert!(!own.iter().any(|(z, _)| *z == TabDrop::Into(left)));
    }
}
