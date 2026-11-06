//! An open file's undo history: a tree of steps, each a list of text
//! changes. Undoing to a step and editing adds a branch; the old one stays.

use std::collections::{HashMap, HashSet};

use super::buffer::AutoClosers;

/// A place in the text: (row, col), col counting chars into the row.
pub type Pos = (usize, usize);

/// One text change: `del` removed at `at`, then `ins` inserted there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Op {
    pub at: Pos,
    pub del: String,
    pub ins: String,
}

impl Op {
    pub fn inverse(&self) -> Op {
        Op { at: self.at, del: self.ins.clone(), ins: self.del.clone() }
    }

    /// Apply to `lines`; returns where the inserted text ends.
    pub fn apply(&self, lines: &mut Vec<String>) -> Pos {
        splice(lines, self.at, end_of(self.at, &self.del), &self.ins).1
    }

    /// What it holds in memory, roughly: its text and [`OP_BYTES`].
    fn bytes(&self) -> usize {
        OP_BYTES + self.del.len() + self.ins.len()
    }
}

/// Where `text` ends when it starts at `at`.
pub(super) fn end_of(at: Pos, text: &str) -> Pos {
    match text.rfind('\n') {
        Some(i) => (at.0 + text.matches('\n').count(), text[i + 1..].chars().count()),
        None => (at.0, at.1 + text.chars().count()),
    }
}

/// Byte offset of char `col` in `line`, its end past the last char.
fn byte_of(line: &str, col: usize) -> usize {
    line.char_indices().nth(col).map_or(line.len(), |(b, _)| b)
}

/// Replace `from..to` of `lines` with `text` (which may hold newlines).
/// Returns the text removed and where the inserted text ends.
pub(super) fn splice(lines: &mut Vec<String>, from: Pos, to: Pos, text: &str) -> (String, Pos) {
    let (b0, b1) = (byte_of(&lines[from.0], from.1), byte_of(&lines[to.0], to.1));
    let removed = if from.0 == to.0 {
        lines[from.0][b0..b1].to_string()
    } else {
        let mut r = lines[from.0][b0..].to_string();
        for line in &lines[from.0 + 1..to.0] {
            r.push('\n');
            r.push_str(line);
        }
        r.push('\n');
        r.push_str(&lines[to.0][..b1]);
        r
    };
    let tail = lines[to.0][b1..].to_string();
    let mut parts = text.split('\n');
    let mut first = std::mem::take(&mut lines[from.0]);
    first.truncate(b0);
    first.push_str(parts.next().unwrap_or(""));
    let mut new = vec![first];
    new.extend(parts.map(str::to_string));
    let row = from.0 + new.len() - 1;
    let last = new.last_mut().expect("one line at least");
    let end = (row, last.chars().count());
    last.push_str(&tail);
    lines.splice(from.0..=to.0, new);
    (removed, end)
}

/// Where position `p` moves when `op` applies (another view's caret): ahead
/// of the change it stays, inside the removed text it goes to the change's
/// start, after it it shifts with the text.
pub(super) fn shift(p: Pos, op: &Op) -> Pos {
    let del_end = end_of(op.at, &op.del);
    let ins_end = end_of(op.at, &op.ins);
    if p <= op.at {
        p
    } else if p < del_end {
        op.at
    } else if p.0 == del_end.0 {
        (ins_end.0, ins_end.1 + p.1 - del_end.1)
    } else {
        (p.0 + ins_end.0 - del_end.0, p.1)
    }
}

pub use crate::plugin::undo_panel::StepId as NodeId;

/// A view's cursor, selection anchor, and auto-inserted closers.
#[derive(Clone, Debug, Default)]
pub(super) struct Caret {
    pub cursor: Pos,
    pub anchor: Option<Pos>,
    pub closers: AutoClosers,
}

/// One step: the changes from its parent's text to its own.
#[derive(Clone, Debug)]
pub struct Node {
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    /// The child redo goes to: the one last made or visited.
    redo: Option<NodeId>,
    ops: Vec<Op>,
    /// The caret before the step's first change and after its last.
    pub(super) before: Caret,
    pub(super) after: Caret,
    /// The text predates a reload no save has followed, or the file became
    /// unreadable: saving it could lose what is on disk.
    pub(super) stale: bool,
}

impl Node {
    fn new(parent: Option<NodeId>, before: Caret, stale: bool) -> Node {
        Node { parent, children: Vec::new(), redo: None, ops: Vec::new(), after: before.clone(), before, stale }
    }

    pub fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    /// Oldest first.
    pub fn children(&self) -> &[NodeId] {
        &self.children
    }

    pub fn redo_child(&self) -> Option<NodeId> {
        self.redo
    }

    pub(super) fn ops(&self) -> &[Op] {
        &self.ops
    }
}

/// What a step costs beyond its changes, for [`UndoTree::prune`].
const NODE_BYTES: usize = 128;

/// What a change costs beyond its text.
const OP_BYTES: usize = 64;

/// The steps of one file's history. The root is the oldest text kept;
/// `current` is the text shown.
#[derive(Clone, Debug)]
pub struct UndoTree {
    nodes: HashMap<NodeId, Node>,
    root: NodeId,
    current: NodeId,
    /// The step whose text is on disk, if it is still in the tree.
    saved: Option<NodeId>,
    next: u64,
    bytes: usize,
}

impl Default for UndoTree {
    fn default() -> Self {
        let root = NodeId(0);
        UndoTree {
            nodes: HashMap::from([(root, Node::new(None, Caret::default(), false))]),
            root,
            current: root,
            saved: Some(root),
            next: 1,
            bytes: NODE_BYTES,
        }
    }
}

impl UndoTree {
    pub fn root(&self) -> NodeId {
        self.root
    }

    pub fn current(&self) -> NodeId {
        self.current
    }

    pub fn saved(&self) -> Option<NodeId> {
        self.saved
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id)
    }

    /// About how many bytes the steps hold; past [`UndoTree::prune`]'s cap
    /// the oldest go.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// How many steps the tree holds, the root included.
    pub fn steps(&self) -> usize {
        self.nodes.len()
    }

    fn cur(&self) -> &Node {
        &self.nodes[&self.current]
    }

    pub(super) fn cur_mut(&mut self) -> &mut Node {
        self.nodes.get_mut(&self.current).expect("the current step is in the tree")
    }

    /// The current step's id and how many changes it has.
    pub(super) fn tip(&self) -> (NodeId, usize) {
        (self.current, self.cur().ops.len())
    }

    /// Start a step after the current one and make it current. It inherits
    /// the current text's staleness.
    pub(super) fn push(&mut self, before: Caret, cap: usize) {
        self.prune(cap);
        let id = NodeId(self.next);
        self.next += 1;
        let parent = self.current;
        let stale = self.cur().stale;
        self.nodes.insert(id, Node::new(Some(parent), before, stale));
        let p = self.cur_mut();
        p.children.push(id);
        p.redo = Some(id);
        self.current = id;
        self.bytes += NODE_BYTES;
    }

    /// Add `op` to the current step.
    pub(super) fn record(&mut self, op: Op) {
        self.bytes += op.bytes();
        self.cur_mut().ops.push(op);
    }

    /// Make `target` the current step; returns the changes that take the
    /// text there from the current one, `None` (nothing moved) when
    /// `target` is not in the tree. Redo from each step above `target` then
    /// leads to it, so redo retraces the branch last visited.
    pub(super) fn jump(&mut self, target: NodeId) -> Option<Vec<Op>> {
        if !self.nodes.contains_key(&target) {
            return None;
        }
        let from = self.tip();
        // Every step above the current one leads to it, so a step to the
        // parent or to a child sets one link; a jump sets them all.
        if self.nodes[&target].parent == Some(from.0) {
            self.cur_mut().redo = Some(target);
        } else if self.cur().parent != Some(target) {
            let mut child = target;
            while let Some(p) = self.nodes[&child].parent {
                self.nodes.get_mut(&p).expect("the parent is in the tree").redo = Some(child);
                child = p;
            }
        }
        self.current = target;
        self.route(from)
    }

    pub(super) fn set_saved(&mut self) {
        self.saved = Some(self.current);
    }

    /// Mark every step's text stale, or none.
    pub(super) fn mark_stale(&mut self, stale: bool) {
        self.nodes.values_mut().for_each(|n| n.stale = stale);
    }

    /// The changes that take a text at `from` (a step and how many of its
    /// changes were applied) to the current text: back up to the common
    /// ancestor, then down. `None` when `from` is no longer in the tree.
    pub(super) fn route(&self, (node, done): (NodeId, usize)) -> Option<Vec<Op>> {
        let n = self.nodes.get(&node)?;
        if done > n.ops.len() {
            return None;
        }
        if node == self.current {
            return Some(n.ops[done..].to_vec());
        }
        // One step back or forward: no walk.
        if n.parent == Some(self.current) {
            return Some(n.ops[..done].iter().rev().map(Op::inverse).collect());
        }
        let cur = self.cur();
        if done == n.ops.len() && cur.parent == Some(node) {
            return Some(cur.ops.clone());
        }
        // A step applied in part (it grew since) is backed out to its parent.
        let (mut ops, mut at): (Vec<Op>, _) = if done == n.ops.len() {
            (Vec::new(), node)
        } else {
            (n.ops[..done].iter().rev().map(Op::inverse).collect(), n.parent?)
        };
        let mut down = vec![self.current];
        while let Some(p) = self.nodes[down.last().expect("not empty")].parent {
            down.push(p);
        }
        let ancestors: HashSet<NodeId> = down.iter().copied().collect();
        while !ancestors.contains(&at) {
            let n = &self.nodes[&at];
            ops.extend(n.ops.iter().rev().map(Op::inverse));
            at = n.parent?;
        }
        let below = down.iter().position(|id| *id == at).expect("an ancestor");
        for id in down[..below].iter().rev() {
            ops.extend(self.nodes[id].ops.iter().cloned());
        }
        Some(ops)
    }

    /// While the tree holds over `cap` bytes, drop the root: the step after
    /// it toward the current one becomes the root (its text the oldest
    /// kept), and the root's other branches go.
    pub(super) fn prune(&mut self, cap: usize) {
        if self.bytes <= cap {
            return;
        }
        let mut path = vec![self.current];
        while let Some(p) = self.nodes[path.last().expect("not empty")].parent {
            path.push(p);
        }
        path.pop();
        while self.bytes > cap {
            let Some(keep) = path.pop() else { break };
            let old = std::mem::replace(&mut self.root, keep);
            let gone = self.nodes.remove(&old).expect("the root is in the tree");
            self.bytes -= NODE_BYTES + gone.ops.iter().map(Op::bytes).sum::<usize>();
            for c in gone.children.into_iter().filter(|c| *c != keep) {
                self.remove_subtree(c);
            }
            let k = self.nodes.get_mut(&keep).expect("on the path");
            k.parent = None;
            self.bytes -= k.ops.iter().map(Op::bytes).sum::<usize>();
            k.ops.clear();
        }
        if self.saved.is_some_and(|s| !self.nodes.contains_key(&s)) {
            self.saved = None;
        }
    }

    fn remove_subtree(&mut self, id: NodeId) {
        let mut stack = vec![id];
        while let Some(id) = stack.pop() {
            if let Some(n) = self.nodes.remove(&id) {
                self.bytes -= NODE_BYTES + n.ops.iter().map(Op::bytes).sum::<usize>();
                stack.extend(n.children);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.split('\n').map(str::to_string).collect()
    }

    #[test]
    fn splice_replaces_across_lines_and_its_inverse_restores() {
        let mut l = lines("one\ntwo\nthree");
        let (removed, end) = splice(&mut l, (0, 1), (2, 2), "X\nY");
        assert_eq!(removed, "ne\ntwo\nth");
        assert_eq!(l, lines("oX\nYree"));
        assert_eq!(end, (1, 1));
        let op = Op { at: (0, 1), del: removed, ins: "X\nY".into() };
        op.inverse().apply(&mut l);
        assert_eq!(l, lines("one\ntwo\nthree"));
        op.apply(&mut l);
        assert_eq!(l, lines("oX\nYree"));
    }

    #[test]
    fn another_views_caret_shifts_with_a_change() {
        let op = Op { at: (1, 2), del: "ab".into(), ins: "x\nyz".into() };
        assert_eq!(shift((1, 2), &op), (1, 2), "at the change: stays");
        assert_eq!(shift((1, 3), &op), (1, 2), "inside what went: to its start");
        assert_eq!(shift((1, 6), &op), (2, 4), "after it on its line");
        assert_eq!(shift((3, 1), &op), (4, 1), "on a later line");
    }

    /// A tree root → a → b, and c branching off a.
    fn tree() -> (UndoTree, [NodeId; 4]) {
        let mut t = UndoTree::default();
        let root = t.root();
        let op = |s: &str| Op { at: (0, 0), del: String::new(), ins: s.into() };
        t.push(Caret::default(), usize::MAX);
        t.record(op("a"));
        let a = t.current();
        t.push(Caret::default(), usize::MAX);
        t.record(op("b"));
        let b = t.current();
        t.jump(a);
        t.push(Caret::default(), usize::MAX);
        t.record(op("c"));
        let c = t.current();
        (t, [root, a, b, c])
    }

    #[test]
    fn a_route_backs_up_to_the_common_step_and_goes_down() {
        let (t, [_, a, b, c]) = tree();
        assert_eq!(t.node(a).unwrap().children(), [b, c]);
        let mut text = lines("ba");
        for op in t.route((b, 1)).unwrap() {
            op.apply(&mut text);
        }
        assert_eq!(text, lines("ca"), "b undone, c applied");
        assert_eq!(t.route((c, 0)).unwrap().len(), 1, "the rest of the current step");
    }

    #[test]
    fn pruning_drops_the_oldest_steps_and_other_branches() {
        let (mut t, [root, a, b, c]) = tree();
        t.prune(t.bytes - 1);
        assert_eq!(t.root(), a, "the root went");
        assert!(t.node(root).is_none() && t.node(a).unwrap().ops().is_empty());
        assert_eq!(t.saved(), None, "the saved text went with it");
        t.prune(0);
        assert_eq!(t.root(), c, "pruned up to the current step");
        assert!(t.node(b).is_none(), "the other branch went");
        assert_eq!(t.steps(), 1);
        assert_eq!(t.bytes, NODE_BYTES);
    }
}
