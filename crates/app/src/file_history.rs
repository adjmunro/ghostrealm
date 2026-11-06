//! The file operations made in one workspace (create, rename, move, Trash)
//! as an undo tree, and the checks each undo and redo makes before it acts.
//! A check reads the filesystem and says what to do ([`Act`]) or why not; it
//! never changes anything. Any doubt (an error, a missing or different entry,
//! a taken name) refuses, so a step acts only on the entry it recorded.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::file_ops::{same_entry_recased, Id};
use crate::plugin::undo_panel::{History, StepId};

/// What a created file looked like: writing to it changes one of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fresh {
    pub len: u64,
    pub modified: Option<SystemTime>,
}

impl Fresh {
    pub fn of(meta: &std::fs::Metadata) -> Fresh {
        Fresh { len: meta.len(), modified: meta.modified().ok() }
    }
}

/// One file operation. An `id` of `None` is an entry whose identity
/// couldn't be read after it was acted on: every check of it refuses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileOp {
    /// A new file (with `fresh`) or folder at `path`.
    Create { path: PathBuf, id: Option<Id>, fresh: Option<Fresh> },
    /// `from` renamed to `to`, or moved (a cut's Paste) when `moved`.
    Rename { from: PathBuf, to: PathBuf, id: Option<Id>, moved: bool },
    /// `path` moved to the Trash, which put it at `at`. `restored`: put back
    /// on its own, out of order, while the step is still in effect.
    Trash { path: PathBuf, at: Option<PathBuf>, id: Option<Id>, restored: bool },
}

/// What an undo or redo does once its checks pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Act {
    /// Its result is already in place: nothing to do.
    Done,
    /// Rename `from` to `to`, refusing if anything is at `to`. `recased`:
    /// both name the same entry, and only the case of its name changes.
    Rename { from: PathBuf, to: PathBuf, recased: bool },
    /// Move `path` to the Trash.
    Trash(PathBuf),
    /// Create `path`, a folder when `folder`, refusing if it is taken.
    Create { path: PathBuf, folder: bool },
}

/// `path`'s last component, for messages.
pub fn name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// The entry at `path`, `None` when nothing is there. Any other error
/// (unreadable, not a folder on the way) refuses.
fn entry(path: &Path) -> Result<Option<Id>, String> {
    match Id::of(path) {
        Ok(id) => Ok(Some(id)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("can't read {}: {e}", path.display())),
    }
}

/// Check that `path` still holds the entry `id`.
fn holds(path: &Path, id: Option<Id>) -> Result<(), String> {
    let id = id.ok_or_else(|| format!("which entry {} is wasn't recorded", name(path)))?;
    match entry(path)? {
        Some(now) if now == id => Ok(()),
        Some(_) => Err(format!("{} was replaced by another entry", name(path))),
        None => Err(format!("{} is gone", name(path))),
    }
}

/// Moving the entry `id` from `from` to `to`: done already when `to` holds
/// it and `from` is free; else `from` must hold it and `to` be free.
fn rename_act(from: &Path, to: &Path, id: Option<Id>) -> Result<Act, String> {
    if id.is_some() && same_entry_recased(from, to) {
        holds(from, id)?;
        return Ok(Act::Rename { from: from.into(), to: to.into(), recased: true });
    }
    if id.is_some() && entry(from)?.is_none() && entry(to)? == id {
        return Ok(Act::Done);
    }
    holds(from, id)?;
    match entry(to)? {
        None => Ok(Act::Rename { from: from.into(), to: to.into(), recased: false }),
        Some(_) => Err(format!("{} already exists", name(to))),
    }
}

impl FileOp {
    /// What undoing it does, or why it can't.
    pub fn undo_act(&self) -> Result<Act, String> {
        match self {
            FileOp::Create { path, id, fresh } => {
                if entry(path)?.is_none() {
                    return Ok(Act::Done);
                }
                holds(path, *id)?;
                let changed = || format!("{} changed since it was created", name(path));
                match fresh {
                    Some(fresh) => {
                        let meta = path.symlink_metadata().map_err(|e| format!("can't read {}: {e}", path.display()))?;
                        if Fresh::of(&meta) != *fresh {
                            return Err(changed());
                        }
                    }
                    None => {
                        let mut list =
                            std::fs::read_dir(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
                        if list.next().is_some() {
                            return Err(format!("{} isn't empty", name(path)));
                        }
                    }
                }
                Ok(Act::Trash(path.clone()))
            }
            FileOp::Rename { from, to, id, .. } => rename_act(to, from, *id),
            FileOp::Trash { path, at, id, .. } => {
                let at = at.as_deref().ok_or("the Trash didn't say where it put it")?;
                rename_act(at, path, *id)
            }
        }
    }

    /// What redoing it does, or why it can't.
    pub fn redo_act(&self) -> Result<Act, String> {
        match self {
            FileOp::Create { path, fresh, .. } => match entry(path)? {
                None => Ok(Act::Create { path: path.clone(), folder: fresh.is_none() }),
                Some(_) => Err(format!("{} already exists", name(path))),
            },
            FileOp::Rename { from, to, id, .. } => rename_act(from, to, *id),
            FileOp::Trash { path, at, id, .. } => {
                let trashed = at.as_deref().is_some_and(|at| entry(at).ok().flatten() == *id);
                if id.is_some() && trashed && entry(path)?.is_none() {
                    return Ok(Act::Done);
                }
                holds(path, *id)?;
                Ok(Act::Trash(path.clone()))
            }
        }
    }

    /// The step's row in the undo panel.
    pub fn label(&self) -> String {
        match self {
            FileOp::Create { path, fresh: Some(_), .. } => format!("New file {}", name(path)),
            FileOp::Create { path, .. } => format!("New folder {}", name(path)),
            FileOp::Rename { from, to, moved: false, .. } => format!("Rename {} \u{2192} {}", name(from), name(to)),
            FileOp::Rename { from, to, .. } => {
                let into = to.parent().map_or_else(String::new, name);
                format!("Move {} \u{2192} {into}", name(from))
            }
            FileOp::Trash { path, .. } => format!("Trash {}", name(path)),
        }
    }
}

/// What a file sidebar asks of its workspace's history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Walk {
    Undo,
    Redo,
    /// Undo and redo to this step.
    To(StepId),
    /// Put this Trash step's entry back on its own, leaving the steps as
    /// they are ([`FileHistory::restorable`]).
    Restore(StepId),
}

/// One step of an undo walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    Undo(StepId),
    Redo(StepId),
}

struct Node {
    parent: Option<StepId>,
    children: Vec<StepId>,
    /// The child redo goes to: the one last made or visited.
    redo: Option<StepId>,
    /// `None` for the root.
    op: Option<FileOp>,
}

/// A workspace's file operations. The root is "Start"; `current` is the last
/// step in effect.
pub struct FileHistory {
    nodes: HashMap<StepId, Node>,
    root: StepId,
    current: StepId,
    next: u64,
}

impl Default for FileHistory {
    fn default() -> Self {
        let root = StepId(0);
        let node = Node { parent: None, children: Vec::new(), redo: None, op: None };
        FileHistory { nodes: HashMap::from([(root, node)]), root, current: root, next: 1 }
    }
}

impl FileHistory {
    /// Add `op` after the current step and make it current.
    pub fn record(&mut self, op: FileOp) -> StepId {
        let id = StepId(self.next);
        self.next += 1;
        let parent = self.current;
        self.nodes.insert(id, Node { parent: Some(parent), children: Vec::new(), redo: None, op: Some(op) });
        let p = self.nodes.get_mut(&parent).expect("the current step is in the tree");
        p.children.push(id);
        p.redo = Some(id);
        self.current = id;
        id
    }

    pub fn op(&self, id: StepId) -> Option<&FileOp> {
        self.nodes.get(&id)?.op.as_ref()
    }

    pub fn op_mut(&mut self, id: StepId) -> Option<&mut FileOp> {
        self.nodes.get_mut(&id)?.op.as_mut()
    }

    /// The step undo reverses: the current one, unless that is the root.
    pub fn undo_step(&self) -> Option<StepId> {
        (self.current != self.root).then_some(self.current)
    }

    /// The step redo applies.
    pub fn redo_step(&self) -> Option<StepId> {
        self.nodes[&self.current].redo
    }

    /// `id` and the steps before it, back to the root.
    fn lineage(&self, id: StepId) -> Vec<StepId> {
        let mut out = vec![id];
        while let Some(p) = self.nodes.get(out.last().expect("not empty")).and_then(|n| n.parent) {
            out.push(p);
        }
        out
    }

    /// Whether `id`'s operation is in effect: it is the current step or one
    /// before it.
    pub fn in_effect(&self, id: StepId) -> bool {
        id != self.root && self.lineage(self.current).contains(&id)
    }

    /// Whether step `id` is a Trash in effect whose entry can be put back
    /// on its own.
    pub fn restorable(&self, id: StepId) -> bool {
        matches!(self.op(id), Some(FileOp::Trash { at: Some(_), restored: false, .. })) && self.in_effect(id)
    }

    /// The steps from the current one to `target`: undo up to their common
    /// ancestor, then redo down. Empty when `target` isn't in the tree.
    pub fn route(&self, target: StepId) -> Vec<Move> {
        if !self.nodes.contains_key(&target) {
            return Vec::new();
        }
        let up = self.lineage(self.current);
        let down = self.lineage(target);
        let common = up.iter().position(|id| down.contains(id)).expect("both reach the root");
        let below = down.iter().position(|id| *id == up[common]).expect("an ancestor");
        let undo = up[..common].iter().map(|id| Move::Undo(*id));
        undo.chain(down[..below].iter().rev().map(|id| Move::Redo(*id))).collect()
    }

    /// Step `id` was undone: its parent is current, and redo leads back to it.
    pub fn undone(&mut self, id: StepId) {
        if let Some(p) = self.nodes.get(&id).and_then(|n| n.parent) {
            self.nodes.get_mut(&p).expect("the parent is in the tree").redo = Some(id);
            self.current = p;
        }
    }

    /// Step `id` was redone: it is current.
    pub fn redone(&mut self, id: StepId) {
        if let Some(p) = self.nodes.get(&id).and_then(|n| n.parent) {
            self.nodes.get_mut(&p).expect("the parent is in the tree").redo = Some(id);
            self.current = id;
        }
    }
}

impl History for FileHistory {
    fn root(&self) -> StepId {
        self.root
    }

    fn current(&self) -> StepId {
        self.current
    }

    fn saved(&self) -> Option<StepId> {
        None
    }

    fn steps(&self) -> usize {
        self.nodes.len()
    }

    fn parent(&self, id: StepId) -> Option<StepId> {
        self.nodes.get(&id)?.parent
    }

    fn children(&self, id: StepId) -> &[StepId] {
        self.nodes.get(&id).map_or(&[], |n| &n.children)
    }

    fn label(&self, id: StepId) -> String {
        match self.op(id) {
            Some(op) => op.label(),
            None => "Start".into(),
        }
    }

    fn note(&self, id: StepId) -> Option<&'static str> {
        matches!(self.op(id), Some(FileOp::Trash { restored: true, .. })).then_some("restored")
    }

    fn action(&self, id: StepId) -> Option<&'static str> {
        self.restorable(id).then_some("Restore")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::testing::tmpdir;

    fn rename(from: &str, to: &str) -> FileOp {
        FileOp::Rename { from: from.into(), to: to.into(), id: None, moved: false }
    }

    #[test]
    fn a_route_undoes_to_the_common_step_and_redoes_down_another_branch() {
        let mut h = FileHistory::default();
        let root = h.root();
        let a = h.record(rename("/w/a", "/w/b"));
        let b = h.record(rename("/w/b", "/w/c"));
        h.undone(b);
        assert_eq!((h.current(), h.redo_step()), (a, Some(b)));
        let c = h.record(rename("/w/b", "/w/d"));
        assert_eq!(h.children(a), [b, c], "a new step after an undo branches, keeping the old branch");
        assert_eq!(h.route(b), [Move::Undo(c), Move::Redo(b)]);
        assert_eq!(h.route(root), [Move::Undo(c), Move::Undo(a)]);
        assert_eq!(h.route(c), []);
        assert!(h.in_effect(a) && h.in_effect(c) && !h.in_effect(b) && !h.in_effect(root));
        h.undone(c);
        h.redone(b);
        assert_eq!((h.current(), h.redo_step()), (b, None));
        h.undone(b);
        assert_eq!(h.redo_step(), Some(b), "redo retraces the branch last visited");
        assert_eq!(h.undo_step(), Some(a));
        h.undone(a);
        assert_eq!(h.undo_step(), None, "nothing before the start");
    }

    #[test]
    fn labels_name_the_operation_and_its_entries() {
        let mut h = FileHistory::default();
        assert_eq!(h.label(h.root()), "Start");
        let ops = [
            (FileOp::Create { path: "/w/a.md".into(), id: None, fresh: Some(Fresh { len: 0, modified: None }) }, "New file a.md"),
            (FileOp::Create { path: "/w/src".into(), id: None, fresh: None }, "New folder src"),
            (rename("/w/a.md", "/w/b.md"), "Rename a.md \u{2192} b.md"),
            (FileOp::Rename { from: "/w/a.md".into(), to: "/w/docs/a.md".into(), id: None, moved: true }, "Move a.md \u{2192} docs"),
            (FileOp::Trash { path: "/w/a.md".into(), at: None, id: None, restored: false }, "Trash a.md"),
        ];
        for (op, label) in ops {
            let id = h.record(op);
            assert_eq!(h.label(id), label);
        }
    }

    #[test]
    fn a_rename_s_checks_find_it_done_or_refuse_a_taken_or_replaced_entry() {
        let d = tmpdir("history-rename-act");
        std::fs::write(d.join("b"), "B").unwrap();
        let id = Id::of(&d.join("b")).ok();
        let op = FileOp::Rename { from: d.join("a"), to: d.join("b"), id, moved: false };
        let back = Act::Rename { from: d.join("b"), to: d.join("a"), recased: false };
        assert_eq!(op.undo_act(), Ok(back));
        assert_eq!(op.redo_act(), Ok(Act::Done), "already in effect");
        std::fs::write(d.join("a"), "A").unwrap();
        assert_eq!(op.undo_act(), Err("a already exists".into()));
        let unknown = FileOp::Rename { from: d.join("x"), to: d.join("b"), id: None, moved: false };
        assert!(unknown.undo_act().is_err(), "an unrecorded identity refuses");
    }
}
