//! The workspace model: vertical tabs, each owning a recursive split tree of
//! panes, each pane owning a strip of terminal surfaces (horizontal tabs).
//!
//! Terminals themselves live in the app (keyed by [`SurfaceId`]); this model is
//! pure structure and is tested headless. Vocabulary follows the glossary:
//! *vtab* (vertical tab / workspace), *pane* (a split panel), and *surface*
//! (one terminal / horizontal tab within a pane).

/// Inbox state of a vtab. The full clearing/hoisting machine lands in a later
/// phase; this is the state it operates on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabStatus {
    Read,
    Busy,
    /// A command completed while the tab was unfocused.
    Unread {
        success: bool,
    },
    /// Running but blocked on the user. Sticky until resolved.
    NeedsInput,
}

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub u64);
    };
}
id_type!(VtabId);
id_type!(PaneId);
id_type!(SurfaceId);

/// A single terminal within a pane's horizontal tab strip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Surface {
    pub id: SurfaceId,
    pub title: String,
    /// A user-set title is sticky and wins over program-set titles.
    pub user_named: bool,
}

/// A split panel: a strip of surfaces with one active.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pane {
    pub id: PaneId,
    pub surfaces: Vec<Surface>,
    pub active: usize,
}

impl Pane {
    pub fn active_surface(&self) -> Option<&Surface> {
        self.surfaces.get(self.active)
    }
}

/// How a split arranges its two children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    /// Children side by side (a vertical divider).
    LeftRight,
    /// Children stacked (a horizontal divider).
    TopBottom,
}

/// A pixel rectangle (used for pane layout). Backend/GPU-agnostic.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// A node in a vtab's binary split tree.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Leaf(Pane),
    Split {
        axis: Axis,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

impl Node {
    /// Compute the pixel rect of every leaf pane within `rect`, splitting by the
    /// stored ratios and leaving a `gap` between split children (the divider).
    pub fn layout(&self, rect: Rect, gap: f32) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        self.layout_into(rect, gap, &mut out);
        out
    }

    fn layout_into(&self, rect: Rect, gap: f32, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            Node::Leaf(p) => out.push((p.id, rect)),
            Node::Split {
                axis,
                ratio,
                first,
                second,
            } => match axis {
                Axis::LeftRight => {
                    let avail = (rect.w - gap).max(0.0);
                    let fw = (avail * ratio).max(0.0);
                    let sw = (avail - fw).max(0.0);
                    first.layout_into(
                        Rect {
                            x: rect.x,
                            y: rect.y,
                            w: fw,
                            h: rect.h,
                        },
                        gap,
                        out,
                    );
                    second.layout_into(
                        Rect {
                            x: rect.x + fw + gap,
                            y: rect.y,
                            w: sw,
                            h: rect.h,
                        },
                        gap,
                        out,
                    );
                }
                Axis::TopBottom => {
                    let avail = (rect.h - gap).max(0.0);
                    let fh = (avail * ratio).max(0.0);
                    let sh = (avail - fh).max(0.0);
                    first.layout_into(
                        Rect {
                            x: rect.x,
                            y: rect.y,
                            w: rect.w,
                            h: fh,
                        },
                        gap,
                        out,
                    );
                    second.layout_into(
                        Rect {
                            x: rect.x,
                            y: rect.y + fh + gap,
                            w: rect.w,
                            h: sh,
                        },
                        gap,
                        out,
                    );
                }
            },
        }
    }

    fn collect_panes<'a>(&'a self, out: &mut Vec<&'a Pane>) {
        match self {
            Node::Leaf(p) => out.push(p),
            Node::Split { first, second, .. } => {
                first.collect_panes(out);
                second.collect_panes(out);
            }
        }
    }

    fn find_pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        match self {
            Node::Leaf(p) if p.id == id => Some(p),
            Node::Leaf(_) => None,
            Node::Split { first, second, .. } => {
                first.find_pane_mut(id).or_else(|| second.find_pane_mut(id))
            }
        }
    }

    /// Replace the leaf `target` with a horizontal/vertical split of itself and
    /// `new_leaf`. Returns true if the target was found.
    fn split_leaf(&mut self, target: PaneId, axis: Axis, new_leaf: Pane) -> bool {
        match self {
            Node::Leaf(p) if p.id == target => {
                // Move the existing leaf into `first`, add the new one as `second`.
                let existing = std::mem::replace(
                    self,
                    Node::Leaf(Pane {
                        id: PaneId(0),
                        surfaces: Vec::new(),
                        active: 0,
                    }),
                );
                *self = Node::Split {
                    axis,
                    ratio: 0.5,
                    first: Box::new(existing),
                    second: Box::new(Node::Leaf(new_leaf)),
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { first, second, .. } => {
                first.split_leaf(target, axis, new_leaf.clone())
                    || second.split_leaf(target, axis, new_leaf)
            }
        }
    }

    /// Remove the leaf `target`, collapsing the parent split into the sibling.
    /// Returns the new subtree, or `None` if this whole subtree was the target.
    fn remove_pane(self, target: PaneId) -> Option<Node> {
        match self {
            Node::Leaf(p) => {
                if p.id == target {
                    None
                } else {
                    Some(Node::Leaf(p))
                }
            }
            Node::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                match (first.remove_pane(target), second.remove_pane(target)) {
                    (Some(f), Some(s)) => Some(Node::Split {
                        axis,
                        ratio,
                        first: Box::new(f),
                        second: Box::new(s),
                    }),
                    // One side collapsed to nothing: the other replaces the split.
                    (Some(only), None) | (None, Some(only)) => Some(only),
                    (None, None) => None,
                }
            }
        }
    }
}

/// A vertical tab: a named workspace with a status and its own split tree.
#[derive(Clone, Debug, PartialEq)]
pub struct Vtab {
    pub id: VtabId,
    pub name: String,
    pub user_named: bool,
    pub status: TabStatus,
    pub root: Node,
    pub focused_pane: PaneId,
}

impl Vtab {
    pub fn panes(&self) -> Vec<&Pane> {
        let mut v = Vec::new();
        self.root.collect_panes(&mut v);
        v
    }
    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.root.find_pane_mut(id)
    }
    /// Pixel rect of each pane in this vtab within `rect`.
    pub fn layout(&self, rect: Rect, gap: f32) -> Vec<(PaneId, Rect)> {
        self.root.layout(rect, gap)
    }
}

/// The whole workspace: an ordered list of vtabs with one active.
#[derive(Clone, Debug, PartialEq)]
pub struct Tree {
    vtabs: Vec<Vtab>,
    active: Option<VtabId>,
    next_id: u64,
}

impl Default for Tree {
    fn default() -> Self {
        Tree {
            vtabs: Vec::new(),
            active: None,
            next_id: 1,
        }
    }
}

impl Tree {
    pub fn new() -> Self {
        Self::default()
    }

    fn fresh(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Create a new vtab containing one pane with one surface. Returns the ids so
    /// the app can attach a terminal to the new surface. The new vtab becomes
    /// active.
    pub fn add_vtab(&mut self, name: impl Into<String>) -> (VtabId, PaneId, SurfaceId) {
        let vtab_id = VtabId(self.fresh());
        let pane_id = PaneId(self.fresh());
        let surface_id = SurfaceId(self.fresh());
        let pane = Pane {
            id: pane_id,
            surfaces: vec![Surface {
                id: surface_id,
                title: String::new(),
                user_named: false,
            }],
            active: 0,
        };
        self.vtabs.push(Vtab {
            id: vtab_id,
            name: name.into(),
            user_named: false,
            status: TabStatus::Read,
            root: Node::Leaf(pane),
            focused_pane: pane_id,
        });
        self.active = Some(vtab_id);
        (vtab_id, pane_id, surface_id)
    }

    pub fn vtabs(&self) -> &[Vtab] {
        &self.vtabs
    }

    pub fn active_vtab(&self) -> Option<VtabId> {
        self.active
    }

    pub fn vtab(&self, id: VtabId) -> Option<&Vtab> {
        self.vtabs.iter().find(|v| v.id == id)
    }

    pub fn vtab_mut(&mut self, id: VtabId) -> Option<&mut Vtab> {
        self.vtabs.iter_mut().find(|v| v.id == id)
    }

    pub fn focus_vtab(&mut self, id: VtabId) -> bool {
        if self.vtabs.iter().any(|v| v.id == id) {
            self.active = Some(id);
            true
        } else {
            false
        }
    }

    /// Split `pane` in `vtab`, adding a new pane with one fresh surface. The new
    /// pane becomes the vtab's focused pane. Returns the new ids.
    pub fn split(&mut self, vtab: VtabId, pane: PaneId, axis: Axis) -> Option<(PaneId, SurfaceId)> {
        let new_pane_id = PaneId(self.fresh());
        let new_surface_id = SurfaceId(self.fresh());
        let new_pane = Pane {
            id: new_pane_id,
            surfaces: vec![Surface {
                id: new_surface_id,
                title: String::new(),
                user_named: false,
            }],
            active: 0,
        };
        let v = self.vtab_mut(vtab)?;
        if v.root.split_leaf(pane, axis, new_pane) {
            v.focused_pane = new_pane_id;
            Some((new_pane_id, new_surface_id))
        } else {
            None
        }
    }

    /// Add a surface (horizontal tab) to a pane; it becomes active. Returns its id.
    pub fn add_surface(&mut self, vtab: VtabId, pane: PaneId) -> Option<SurfaceId> {
        let sid = SurfaceId(self.fresh());
        let v = self.vtab_mut(vtab)?;
        let p = v.pane_mut(pane)?;
        p.surfaces.push(Surface {
            id: sid,
            title: String::new(),
            user_named: false,
        });
        p.active = p.surfaces.len() - 1;
        Some(sid)
    }

    /// Close a surface. If it was the pane's last surface, the pane is closed and
    /// the split collapses; if that was the vtab's last pane, the vtab closes.
    /// Returns true if something was removed.
    pub fn close_surface(&mut self, vtab: VtabId, pane: PaneId, surface: SurfaceId) -> bool {
        let Some(v) = self.vtab_mut(vtab) else {
            return false;
        };
        let Some(p) = v.pane_mut(pane) else {
            return false;
        };
        let Some(pos) = p.surfaces.iter().position(|s| s.id == surface) else {
            return false;
        };
        p.surfaces.remove(pos);
        if p.active >= p.surfaces.len() && p.active > 0 {
            p.active = p.surfaces.len().saturating_sub(1);
        }
        if p.surfaces.is_empty() {
            self.close_pane(vtab, pane);
        }
        true
    }

    /// Close a pane, collapsing the split into its sibling. If it was the vtab's
    /// last pane, the vtab is closed.
    pub fn close_pane(&mut self, vtab: VtabId, pane: PaneId) -> bool {
        let Some(v) = self.vtab_mut(vtab) else {
            return false;
        };
        let root = std::mem::replace(
            &mut v.root,
            Node::Leaf(Pane {
                id: PaneId(0),
                surfaces: Vec::new(),
                active: 0,
            }),
        );
        match root.remove_pane(pane) {
            Some(new_root) => {
                v.root = new_root;
                // Keep focus valid.
                let panes = v.panes();
                if !panes.iter().any(|p| p.id == v.focused_pane) {
                    if let Some(first) = panes.first() {
                        v.focused_pane = first.id;
                    }
                }
                true
            }
            None => self.close_vtab(vtab),
        }
    }

    /// Close a vtab. Active selection moves to a neighbour if needed.
    pub fn close_vtab(&mut self, vtab: VtabId) -> bool {
        let Some(pos) = self.vtabs.iter().position(|v| v.id == vtab) else {
            return false;
        };
        self.vtabs.remove(pos);
        if self.active == Some(vtab) {
            self.active = self
                .vtabs
                .get(pos)
                .or_else(|| self.vtabs.get(pos.wrapping_sub(1)))
                .or_else(|| self.vtabs.last())
                .map(|v| v.id);
        }
        true
    }

    /// Set a surface's title. A program-set title (`user_named=false`) never
    /// overwrites a user-set one.
    pub fn set_surface_title(
        &mut self,
        surface: SurfaceId,
        title: impl Into<String>,
        user_named: bool,
    ) {
        for v in &mut self.vtabs {
            if let Some(p) = find_surface_pane(&mut v.root, surface) {
                if let Some(s) = p.surfaces.iter_mut().find(|s| s.id == surface) {
                    if user_named || !s.user_named {
                        s.title = title.into();
                        s.user_named = user_named || s.user_named;
                    }
                    return;
                }
            }
        }
    }

    pub fn set_status(&mut self, vtab: VtabId, status: TabStatus) {
        if let Some(v) = self.vtab_mut(vtab) {
            v.status = status;
        }
    }

    /// Total surface count across all vtabs (for the app to size its terminal map).
    pub fn surface_count(&self) -> usize {
        self.vtabs
            .iter()
            .flat_map(|v| v.panes())
            .map(|p| p.surfaces.len())
            .sum()
    }
}

fn find_surface_pane(node: &mut Node, surface: SurfaceId) -> Option<&mut Pane> {
    match node {
        Node::Leaf(p) => {
            if p.surfaces.iter().any(|s| s.id == surface) {
                Some(p)
            } else {
                None
            }
        }
        Node::Split { first, second, .. } => {
            if let Some(p) = find_surface_pane(first, surface) {
                Some(p)
            } else {
                find_surface_pane(second, surface)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_vtab_creates_one_pane_one_surface() {
        let mut t = Tree::new();
        let (vt, pane, surf) = t.add_vtab("shell");
        assert_eq!(t.active_vtab(), Some(vt));
        let v = t.vtab(vt).unwrap();
        assert_eq!(v.panes().len(), 1);
        assert_eq!(v.focused_pane, pane);
        assert_eq!(v.panes()[0].surfaces[0].id, surf);
    }

    #[test]
    fn split_adds_pane_and_focuses_it() {
        let mut t = Tree::new();
        let (vt, pane, _) = t.add_vtab("a");
        let (new_pane, _new_surf) = t.split(vt, pane, Axis::LeftRight).unwrap();
        let v = t.vtab(vt).unwrap();
        assert_eq!(v.panes().len(), 2);
        assert_eq!(v.focused_pane, new_pane);
        assert!(matches!(
            v.root,
            Node::Split {
                axis: Axis::LeftRight,
                ..
            }
        ));
    }

    #[test]
    fn closing_a_pane_collapses_the_split() {
        let mut t = Tree::new();
        let (vt, pane, _) = t.add_vtab("a");
        let (new_pane, _) = t.split(vt, pane, Axis::TopBottom).unwrap();
        assert!(t.close_pane(vt, new_pane));
        let v = t.vtab(vt).unwrap();
        assert_eq!(
            v.panes().len(),
            1,
            "split should collapse to the surviving pane"
        );
        assert!(matches!(v.root, Node::Leaf(_)));
        assert_eq!(
            v.focused_pane, pane,
            "focus should fall back to a live pane"
        );
    }

    #[test]
    fn closing_last_pane_closes_the_vtab() {
        let mut t = Tree::new();
        let (vt, pane, _) = t.add_vtab("only");
        assert!(t.close_pane(vt, pane));
        assert!(t.vtab(vt).is_none());
        assert_eq!(t.active_vtab(), None);
    }

    #[test]
    fn closing_last_surface_closes_the_pane() {
        let mut t = Tree::new();
        let (vt, pane, _) = t.add_vtab("a");
        let (p2, _) = t.split(vt, pane, Axis::LeftRight).unwrap();
        let s2 = t.vtab(vt).unwrap().pane_mut_ref(p2).surfaces[0].id;
        assert!(t.close_surface(vt, p2, s2));
        let v = t.vtab(vt).unwrap();
        assert_eq!(v.panes().len(), 1, "emptying a pane should collapse it");
    }

    #[test]
    fn user_title_sticks_over_program_title() {
        let mut t = Tree::new();
        let (_vt, _pane, surf) = t.add_vtab("a");
        t.set_surface_title(surf, "vim", false); // program title
        t.set_surface_title(surf, "my-editor", true); // user pins it
        t.set_surface_title(surf, "bash", false); // program tries to change it
        let v = &t.vtabs()[0];
        assert_eq!(v.panes()[0].surfaces[0].title, "my-editor");
    }

    #[test]
    fn layout_single_pane_fills_rect() {
        let mut t = Tree::new();
        let (vt, _, _) = t.add_vtab("a");
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            w: 800.0,
            h: 600.0,
        };
        let panes = t.vtab(vt).unwrap().layout(rect, 4.0);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].1, rect);
    }

    #[test]
    fn layout_leftright_splits_width_with_gap() {
        let mut t = Tree::new();
        let (vt, pane, _) = t.add_vtab("a");
        t.split(vt, pane, Axis::LeftRight).unwrap();
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            w: 804.0,
            h: 600.0,
        };
        let panes = t.vtab(vt).unwrap().layout(rect, 4.0);
        assert_eq!(panes.len(), 2);
        // 0.5 ratio of (804-4)=800 → 400 each; second starts after first+gap.
        assert_eq!(
            panes[0].1,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 400.0,
                h: 600.0
            }
        );
        assert_eq!(
            panes[1].1,
            Rect {
                x: 404.0,
                y: 0.0,
                w: 400.0,
                h: 600.0
            }
        );
    }

    #[test]
    fn layout_topbottom_splits_height() {
        let mut t = Tree::new();
        let (vt, pane, _) = t.add_vtab("a");
        t.split(vt, pane, Axis::TopBottom).unwrap();
        let rect = Rect {
            x: 10.0,
            y: 20.0,
            w: 300.0,
            h: 204.0,
        };
        let panes = t.vtab(vt).unwrap().layout(rect, 4.0);
        assert_eq!(
            panes[0].1,
            Rect {
                x: 10.0,
                y: 20.0,
                w: 300.0,
                h: 100.0
            }
        );
        assert_eq!(
            panes[1].1,
            Rect {
                x: 10.0,
                y: 124.0,
                w: 300.0,
                h: 100.0
            }
        );
    }

    #[test]
    fn close_active_vtab_moves_selection() {
        let mut t = Tree::new();
        let (a, _, _) = t.add_vtab("a");
        let (b, _, _) = t.add_vtab("b");
        t.focus_vtab(a);
        t.close_vtab(a);
        assert_eq!(t.active_vtab(), Some(b));
    }

    // Small test-only helper.
    impl Vtab {
        fn pane_mut_ref(&self, id: PaneId) -> &Pane {
            self.panes().into_iter().find(|p| p.id == id).unwrap()
        }
    }
}
