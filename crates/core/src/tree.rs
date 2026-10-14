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
            } => {
                let (a, _, b) = split_rects(*axis, *ratio, rect, gap);
                first.layout_into(a, gap, out);
                second.layout_into(b, gap, out);
            }
        }
    }

    /// Every divider of the split tree laid out in `rect` (see [`Self::layout`]).
    pub fn dividers(&self, rect: Rect, gap: f32) -> Vec<Divider> {
        let mut out = Vec::new();
        self.dividers_into(rect, gap, &mut Vec::new(), &mut out);
        out
    }

    fn dividers_into(&self, rect: Rect, gap: f32, path: &mut SplitPath, out: &mut Vec<Divider>) {
        let Node::Split { axis, ratio, first, second } = self else {
            return;
        };
        let (a, strip, b) = split_rects(*axis, *ratio, rect, gap);
        out.push(Divider { path: path.clone(), axis: *axis, strip, span: rect });
        for (side, child, r) in [(false, first, a), (true, second, b)] {
            path.push(side);
            child.dividers_into(r, gap, path, out);
            path.pop();
        }
    }

    /// The ratio of the split at `path`.
    fn ratio_at_mut(&mut self, path: &[bool]) -> Option<&mut f32> {
        match (self, path.split_first()) {
            (Node::Split { ratio, .. }, None) => Some(ratio),
            (Node::Split { first, second, .. }, Some((&side, rest))) => {
                if side { second } else { first }.ratio_at_mut(rest)
            }
            (Node::Leaf(_), _) => None,
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

/// Divide `rect` for a split: the first child, the divider strip of width
/// `gap`, and the second child. The children share what the strip leaves.
fn split_rects(axis: Axis, ratio: f32, rect: Rect, gap: f32) -> (Rect, Rect, Rect) {
    match axis {
        Axis::LeftRight => {
            let avail = (rect.w - gap).max(0.0);
            let fw = (avail * ratio).max(0.0);
            let sw = (avail - fw).max(0.0);
            (
                Rect { x: rect.x, y: rect.y, w: fw, h: rect.h },
                Rect { x: rect.x + fw, y: rect.y, w: gap, h: rect.h },
                Rect { x: rect.x + fw + gap, y: rect.y, w: sw, h: rect.h },
            )
        }
        Axis::TopBottom => {
            let avail = (rect.h - gap).max(0.0);
            let fh = (avail * ratio).max(0.0);
            let sh = (avail - fh).max(0.0);
            (
                Rect { x: rect.x, y: rect.y, w: rect.w, h: fh },
                Rect { x: rect.x, y: rect.y + fh, w: rect.w, h: gap },
                Rect { x: rect.x, y: rect.y + fh + gap, w: rect.w, h: sh },
            )
        }
    }
}

/// Which split of a vtab's tree: the steps down from the root, `false` into
/// the first child and `true` into the second. It names a split for as long as
/// the tree's shape stays the same.
pub type SplitPath = Vec<bool>;

/// The strip between the two children of a split, which the mouse drags to
/// resize them.
#[derive(Clone, Debug, PartialEq)]
pub struct Divider {
    pub path: SplitPath,
    pub axis: Axis,
    /// The strip itself (the gap left between the children).
    pub strip: Rect,
    /// The whole split: both children and the strip.
    pub span: Rect,
}

impl Divider {
    /// `p` along the direction the divider moves.
    fn along(&self, p: (f32, f32)) -> f32 {
        match self.axis {
            Axis::LeftRight => p.0,
            Axis::TopBottom => p.1,
        }
    }

    /// The strip's position along the direction it moves.
    fn strip_start(&self) -> f32 {
        self.along((self.strip.x, self.strip.y))
    }

    /// The strip's size along the direction it moves.
    fn strip_len(&self) -> f32 {
        match self.axis {
            Axis::LeftRight => self.strip.w,
            Axis::TopBottom => self.strip.h,
        }
    }

    /// How far `p` is from the strip's centre line, across the direction it
    /// moves.
    fn off_centre(&self, p: (f32, f32)) -> f32 {
        (self.along(p) - (self.strip_start() + self.strip_len() * 0.5)).abs()
    }

    /// Whether `p` is on the strip or within `slop` of either side of it (and
    /// within the split's extent along the strip).
    pub fn grabs(&self, slop: f32, p: (f32, f32)) -> bool {
        let across = match self.axis {
            Axis::LeftRight => (self.span.y, self.span.h, p.1),
            Axis::TopBottom => (self.span.x, self.span.w, p.0),
        };
        self.off_centre(p) <= self.strip_len() * 0.5 + slop && across.2 >= across.0 && across.2 < across.0 + across.1
    }

    /// Where along the strip `p` grabbed it (to keep that spot under the
    /// pointer while dragging).
    pub fn grab_offset(&self, p: (f32, f32)) -> f32 {
        self.along(p) - self.strip_start()
    }

    /// The ratio that puts the strip where the pointer `p` drags it, holding
    /// the spot grabbed at `grab` ([`Self::grab_offset`]). Each child keeps at
    /// least `min` along the direction of the split; when the split has no room
    /// for two of those it stays halved.
    pub fn ratio_at(&self, p: (f32, f32), grab: f32, min: f32) -> f32 {
        let start = self.along((self.span.x, self.span.y));
        let len = self.along((self.span.w, self.span.h));
        let avail = len - self.strip_len();
        if avail < 2.0 * min || avail <= 0.0 {
            return 0.5;
        }
        let first = self.along(p) - grab - start;
        (first / avail).clamp(min / avail, 1.0 - min / avail)
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
    /// The directory new terminals/editors in this workspace start in. `None`
    /// falls back to the app's default directory (then the shell's default).
    pub root_dir: Option<std::path::PathBuf>,
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
    /// Every divider in this vtab within `rect`.
    pub fn dividers(&self, rect: Rect, gap: f32) -> Vec<Divider> {
        self.root.dividers(rect, gap)
    }
    /// The divider a pointer at `p` grabs: on its strip or within `slop` of it.
    /// Where grab areas overlap (a split's divider meeting a nested one), the
    /// divider whose centre line is nearest wins.
    pub fn divider_at(&self, rect: Rect, gap: f32, slop: f32, p: (f32, f32)) -> Option<Divider> {
        self.dividers(rect, gap)
            .into_iter()
            .filter(|d| d.grabs(slop, p))
            .min_by(|a, b| a.off_centre(p).total_cmp(&b.off_centre(p)))
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
            root_dir: None,
        });
        self.active = Some(vtab_id);
        (vtab_id, pane_id, surface_id)
    }

    /// Create a new vtab with one empty pane (no surfaces) — the "nothing open"
    /// state. The new vtab becomes active. Returns the vtab and pane ids.
    pub fn add_empty_vtab(&mut self, name: impl Into<String>) -> (VtabId, PaneId) {
        let vtab_id = VtabId(self.fresh());
        let pane_id = PaneId(self.fresh());
        let pane = Pane {
            id: pane_id,
            surfaces: Vec::new(),
            active: 0,
        };
        self.vtabs.push(Vtab {
            id: vtab_id,
            name: name.into(),
            user_named: false,
            status: TabStatus::Read,
            root: Node::Leaf(pane),
            focused_pane: pane_id,
            root_dir: None,
        });
        self.active = Some(vtab_id);
        (vtab_id, pane_id)
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

    /// Split `pane` in `vtab`, adding a new *empty* pane (no surface) — it opens on
    /// the "nothing open" picker. The new pane becomes focused. Returns its id.
    pub fn split_empty(&mut self, vtab: VtabId, pane: PaneId, axis: Axis) -> Option<PaneId> {
        let new_pane_id = PaneId(self.fresh());
        let new_pane = Pane {
            id: new_pane_id,
            surfaces: Vec::new(),
            active: 0,
        };
        let v = self.vtab_mut(vtab)?;
        if v.root.split_leaf(pane, axis, new_pane) {
            v.focused_pane = new_pane_id;
            Some(new_pane_id)
        } else {
            None
        }
    }

    /// Set the first child's share of the split at `path` in
    /// `vtab`. Other splits keep theirs. Returns false for an unknown vtab or
    /// path, or a ratio that isn't a number.
    pub fn set_ratio(&mut self, vtab: VtabId, path: &[bool], ratio: f32) -> bool {
        if !ratio.is_finite() {
            return false;
        }
        match self.vtab_mut(vtab).and_then(|v| v.root.ratio_at_mut(path)) {
            Some(r) => {
                *r = ratio.clamp(0.0, 1.0);
                true
            }
            None => false,
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

    /// Close a surface. If it was the pane's last surface and other panes remain,
    /// the pane closes and the split collapses. If it was the *sole* pane's last
    /// surface, the pane stays as the workspace's empty ("nothing open") state
    /// rather than closing the workspace. Returns true if something was removed.
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
        if pos < p.active {
            // The active surface moved down a place with the ones after `pos`.
            p.active -= 1;
        } else if p.active >= p.surfaces.len() && p.active > 0 {
            p.active = p.surfaces.len().saturating_sub(1);
        }
        if p.surfaces.is_empty() {
            // Collapse into a sibling if there is one; the sole pane is kept empty.
            let sole_pane = v.panes().len() == 1;
            if !sole_pane {
                self.close_pane(vtab, pane);
            }
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
    fn closing_a_surface_before_the_active_one_keeps_the_active_one() {
        let mut t = Tree::new();
        let (vt, pane, first) = t.add_vtab("a");
        let second = t.add_surface(vt, pane).unwrap();
        let third = t.add_surface(vt, pane).unwrap();
        t.vtab_mut(vt).unwrap().pane_mut(pane).unwrap().active = 1;
        let active = |t: &Tree| t.vtab(vt).unwrap().pane_mut_ref(pane).active_surface().unwrap().id;
        assert_eq!(active(&t), second);
        assert!(t.close_surface(vt, pane, first));
        assert_eq!(active(&t), second, "closing an earlier tab");
        assert!(t.close_surface(vt, pane, third));
        assert_eq!(active(&t), second, "closing a later tab");
    }

    #[test]
    fn closing_last_surface_of_sole_pane_leaves_it_empty() {
        let mut t = Tree::new();
        let (vt, pane, s) = t.add_vtab("only");
        assert!(t.close_surface(vt, pane, s));
        let v = t.vtab(vt).expect("sole workspace survives as empty");
        assert_eq!(v.panes().len(), 1, "the sole pane is kept");
        assert!(v.panes()[0].surfaces.is_empty(), "kept pane is now empty");
        assert_eq!(v.focused_pane, pane);
        assert_eq!(t.active_vtab(), Some(vt));
    }

    #[test]
    fn add_empty_vtab_has_a_surfaceless_pane() {
        let mut t = Tree::new();
        let (vt, pane) = t.add_empty_vtab("empty");
        let v = t.vtab(vt).unwrap();
        assert_eq!(v.panes().len(), 1);
        assert_eq!(v.focused_pane, pane);
        assert!(v.panes()[0].surfaces.is_empty());
        assert_eq!(t.active_vtab(), Some(vt));
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

    /// 804x604 workspace: the left pane | (top / bottom) panes. Returns the
    /// tree, vtab, and the left, top, and bottom panes.
    fn left_and_stack() -> (Tree, VtabId, PaneId, PaneId, PaneId) {
        let mut t = Tree::new();
        let (vt, left, _) = t.add_vtab("a");
        let (right, _) = t.split(vt, left, Axis::LeftRight).unwrap();
        let (bottom, _) = t.split(vt, right, Axis::TopBottom).unwrap();
        (t, vt, left, right, bottom)
    }

    const WS: Rect = Rect { x: 0.0, y: 0.0, w: 804.0, h: 604.0 };

    fn width_of(t: &Tree, vt: VtabId, pane: PaneId) -> f32 {
        t.vtab(vt).unwrap().layout(WS, 4.0).into_iter().find(|(id, _)| *id == pane).unwrap().1.w
    }

    #[test]
    fn dividers_sit_on_the_gap_between_the_children_of_each_split() {
        let (t, vt, ..) = left_and_stack();
        let ds = t.vtab(vt).unwrap().dividers(WS, 4.0);
        assert_eq!(ds.len(), 2);
        assert_eq!((ds[0].path.clone(), ds[0].axis), (vec![], Axis::LeftRight));
        assert_eq!(ds[0].strip, Rect { x: 400.0, y: 0.0, w: 4.0, h: 604.0 });
        assert_eq!(ds[0].span, WS);
        assert_eq!((ds[1].path.clone(), ds[1].axis), (vec![true], Axis::TopBottom));
        assert_eq!(ds[1].strip, Rect { x: 404.0, y: 300.0, w: 400.0, h: 4.0 });
        assert_eq!(ds[1].span, Rect { x: 404.0, y: 0.0, w: 400.0, h: 604.0 });
    }

    #[test]
    fn a_single_pane_has_no_dividers() {
        let mut t = Tree::new();
        let (vt, ..) = t.add_vtab("a");
        assert!(t.vtab(vt).unwrap().dividers(WS, 4.0).is_empty());
        assert!(t.vtab(vt).unwrap().divider_at(WS, 4.0, 3.0, (402.0, 10.0)).is_none());
    }

    #[test]
    fn a_pointer_grabs_the_strip_and_a_few_pixels_either_side_of_it() {
        let (t, vt, ..) = left_and_stack();
        let v = t.vtab(vt).unwrap();
        let at = |x, y| v.divider_at(WS, 4.0, 3.0, (x, y)).map(|d| d.path);
        assert_eq!(at(402.0, 10.0), Some(vec![]), "on the strip");
        assert_eq!(at(397.0, 10.0), Some(vec![]), "slop in the first pane");
        assert_eq!(at(406.9, 10.0), Some(vec![]), "slop in the second pane");
        assert_eq!(at(396.9, 10.0), None);
        assert_eq!(at(407.1, 10.0), None);
        assert_eq!(at(600.0, 302.0), Some(vec![true]), "the nested divider");
        assert_eq!(at(600.0, 308.0), None);
        assert_eq!(at(402.0, 700.0), None, "below the split");
    }

    #[test]
    fn where_grab_areas_meet_the_nearer_divider_wins() {
        let (t, vt, ..) = left_and_stack();
        let v = t.vtab(vt).unwrap();
        let at = |x, y| v.divider_at(WS, 4.0, 3.0, (x, y)).map(|d| d.path);
        // Next to the root divider (centre x=402) and beside the nested strip's end.
        assert_eq!(at(405.0, 302.0), Some(vec![true]), "on the nested strip");
        assert_eq!(at(401.0, 302.0), Some(vec![]), "left of the nested split");
        assert_eq!(at(406.0, 290.0), Some(vec![]), "beside the nested strip, off it");
    }

    #[test]
    fn dragging_gives_free_ratios_and_a_pane_keeps_its_minimum() {
        let (t, vt, ..) = left_and_stack();
        let d = t.vtab(vt).unwrap().dividers(WS, 4.0).remove(0);
        // 800 px shared; the pointer sets the first pane's width.
        assert_eq!(d.ratio_at((200.0, 5.0), 0.0, 50.0), 0.25);
        assert_eq!(d.ratio_at((600.0, 5.0), 0.0, 50.0), 0.75);
        assert_eq!(d.ratio_at((0.0, 5.0), 0.0, 50.0), 50.0 / 800.0, "left minimum");
        assert_eq!(d.ratio_at((-500.0, 5.0), 0.0, 50.0), 50.0 / 800.0);
        assert_eq!(d.ratio_at((5000.0, 5.0), 0.0, 50.0), 1.0 - 50.0 / 800.0, "right minimum");
        // The spot grabbed stays under the pointer: grabbed 2 px into the strip.
        let grab = d.grab_offset((402.0, 5.0));
        assert_eq!(grab, 2.0);
        assert_eq!(d.ratio_at((302.0, 5.0), grab, 50.0), 300.0 / 800.0);
        // Pressing and not moving leaves the ratio where it was.
        assert_eq!(d.ratio_at((402.0, 5.0), grab, 50.0), 0.5);
    }

    #[test]
    fn a_split_too_small_for_two_minimums_stays_halved() {
        let (t, vt, ..) = left_and_stack();
        let small = Rect { w: 104.0, ..WS };
        let d = t.vtab(vt).unwrap().dividers(small, 4.0).remove(0);
        assert_eq!(d.ratio_at((10.0, 5.0), 0.0, 60.0), 0.5);
        assert_eq!(d.ratio_at((90.0, 5.0), 0.0, 60.0), 0.5);
    }

    #[test]
    fn a_top_bottom_divider_follows_the_vertical_pointer() {
        let (t, vt, ..) = left_and_stack();
        let d = t.vtab(vt).unwrap().dividers(WS, 4.0).remove(1);
        assert_eq!(d.ratio_at((600.0, 150.0), 0.0, 40.0), 0.25);
        assert_eq!(d.ratio_at((0.0, 1000.0), 0.0, 40.0), 1.0 - 40.0 / 600.0);
    }

    #[test]
    fn set_ratio_resizes_only_the_split_that_owns_the_divider() {
        let (mut t, vt, left, right, bottom) = left_and_stack();
        assert!(t.set_ratio(vt, &[], 0.25));
        assert_eq!(width_of(&t, vt, left), 200.0);
        assert_eq!(width_of(&t, vt, right), 600.0);
        let heights = |t: &Tree| {
            let rects = t.vtab(vt).unwrap().layout(WS, 4.0);
            let h = |id| rects.iter().find(|(p, _)| *p == id).unwrap().1.h;
            (h(right), h(bottom))
        };
        assert_eq!(heights(&t), (300.0, 300.0), "the nested split kept its half");
        assert!(t.set_ratio(vt, &[true], 0.75));
        assert_eq!(heights(&t), (450.0, 150.0));
        assert_eq!(width_of(&t, vt, left), 200.0, "and the outer one kept its ratio");
    }

    #[test]
    fn set_ratio_rejects_unknown_targets_and_bad_numbers_and_clamps() {
        let (mut t, vt, left, ..) = left_and_stack();
        assert!(!t.set_ratio(VtabId(999), &[], 0.3));
        assert!(!t.set_ratio(vt, &[false], 0.3), "a leaf is not a split");
        assert!(!t.set_ratio(vt, &[true, true, true], 0.3));
        assert!(!t.set_ratio(vt, &[], f32::NAN));
        assert!(!t.set_ratio(vt, &[], f32::INFINITY));
        assert_eq!(width_of(&t, vt, left), 400.0, "nothing changed");
        assert!(t.set_ratio(vt, &[], 2.0));
        assert_eq!(width_of(&t, vt, left), 800.0);
        assert!(t.set_ratio(vt, &[], -1.0));
        assert_eq!(width_of(&t, vt, left), 0.0);
    }

    // Small test-only helper.
    impl Vtab {
        fn pane_mut_ref(&self, id: PaneId) -> &Pane {
            self.panes().into_iter().find(|p| p.id == id).unwrap()
        }
    }
}
