//! Undo and redo of file operations: each workspace's [`FileHistory`],
//! recorded by the file commands in `files.rs`, walked from its Files
//! surfaces. Every step checks before it acts ([`FileOp::undo_act`]) and is
//! one no-replace rename, one create, or one move to the Trash; a refusal
//! changes nothing and stops the walk.

use std::cell::RefCell;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use ghostrealm_core::SurfaceId;

use super::AppState;
use crate::file_history::{name, Act, FileHistory, FileOp, Fresh, Move, Walk};
use crate::file_ops::{self, Id};
use crate::plugin::undo_panel::StepId;
use crate::plugins::files::FilesView;
use crate::toasts::{Level, Toast};

/// The key of the toast a walk shows: the next one replaces it.
const TOAST_KEY: &str = "file_undo";

impl AppState {
    /// Walk the file history of surface `sid`'s workspace. A refused step
    /// stops the walk where it is, with a toast saying why. Returns whether
    /// anything was tried (the window repaints).
    pub fn walk_file_history(&mut self, sid: SurfaceId, walk: Walk) -> bool {
        let Some(vt) = self.vtab_of_surface(sid) else { return false };
        let h = self.file_history(vt);
        let route: Vec<Move> = {
            let h = h.borrow();
            match walk {
                Walk::Undo => h.undo_step().map(Move::Undo).into_iter().collect(),
                Walk::Redo => h.redo_step().map(Move::Redo).into_iter().collect(),
                Walk::To(id) => h.route(id),
                Walk::Restore(id) => h.restorable(id).then_some(Move::Undo(id)).into_iter().collect(),
            }
        };
        let Some(first) = route.first().copied() else { return false };
        // A copy or move in flight may be using the paths a step checks.
        if !self.file_jobs_idle() {
            let what = match (walk, first) {
                (Walk::Restore(_), _) => "restore",
                (_, Move::Undo(_)) => "undo",
                _ => "redo",
            };
            self.file_undo_toast(sid, Level::Error, format!("Can't {what} while a file operation is running: wait for it to finish."));
            return true;
        }
        if let Walk::Restore(id) = walk {
            let label = match h.borrow().op(id) {
                Some(FileOp::Trash { path, .. }) => name(path),
                _ => String::new(),
            };
            match self.restore_step(&h, id) {
                Ok(()) => self.file_undo_toast(sid, Level::Info, format!("Restored {label}")),
                Err(why) => self.file_undo_toast(sid, Level::Error, why),
            }
            return true;
        }
        let mut done = None;
        for step in route {
            match self.file_step(&h, step) {
                Ok(label) => done = Some((step, label)),
                Err(why) => {
                    self.file_undo_toast(sid, Level::Error, why);
                    return true;
                }
            }
        }
        // The open panel shows where the walk went.
        let quiet = self.file_undo_panel_open(sid);
        if let (Some((step, label)), Walk::Undo | Walk::Redo, false) = (done, walk, quiet) {
            let verb = if matches!(step, Move::Undo(_)) { "Undid" } else { "Redid" };
            let mut label = label;
            if let Some(first) = label.get(..1) {
                label = first.to_lowercase() + &label[1..];
            }
            self.file_undo_toast(sid, Level::Info, format!("{verb} {label}"));
        }
        true
    }

    /// Whether surface `sid` shows its sidebar's undo panel.
    fn file_undo_panel_open(&self, sid: SurfaceId) -> bool {
        let files = self.view(sid).and_then(|v| v.downcast_ref::<FilesView>());
        files.is_some_and(|f| f.sidebar().undo_panel_open())
    }

    fn file_undo_toast(&mut self, sid: SurfaceId, level: Level, message: String) {
        let toast = Toast::new(level, message).on(self.scope_of(Some(sid))).keyed(TOAST_KEY);
        self.notify(toast);
    }

    /// Check and take one step of `h`. Returns its label, or why it was
    /// refused (nothing changed then).
    fn file_step(&mut self, h: &Rc<RefCell<FileHistory>>, step: Move) -> Result<String, String> {
        let (id, undo) = match step {
            Move::Undo(id) => (id, true),
            Move::Redo(id) => (id, false),
        };
        let op = h.borrow().op(id).cloned().ok_or("the step is gone")?;
        let label = op.label();
        let refused = |why| refusal(if undo { "undo" } else { "redo" }, &op, why);
        let act = if undo { op.undo_act() } else { op.redo_act() }.map_err(refused)?;
        let made = self.file_act(&act).map_err(refused)?;
        let mut h = h.borrow_mut();
        if let Some(op) = h.op_mut(id) {
            note_outcome(op, &act, made);
        }
        if undo {
            h.undone(id);
        } else {
            h.redone(id);
        }
        Ok(label)
    }

    /// Put Trash step `id`'s entry back where it was, with an undo's checks,
    /// and mark it restored. The current step stays.
    fn restore_step(&mut self, h: &Rc<RefCell<FileHistory>>, id: StepId) -> Result<(), String> {
        let op = h.borrow().op(id).cloned().ok_or("the step is gone")?;
        let refused = |why| refusal("restore", &op, why);
        let act = op.undo_act().map_err(refused)?;
        self.file_act(&act).map_err(refused)?;
        if let Some(FileOp::Trash { restored, .. }) = h.borrow_mut().op_mut(id) {
            *restored = true;
        }
        Ok(())
    }

    /// Do `act`. Returns what a create or Trash left: the entry and, for the
    /// Trash, where it went.
    fn file_act(&mut self, act: &Act) -> Result<Made, String> {
        match act {
            Act::Done => Ok(Made::default()),
            Act::Rename { from, to, recased } => {
                let renamed = if *recased { std::fs::rename(from, to) } else { file_ops::rename_exclusive(from, to) };
                renamed.map_err(|e| match e.kind() {
                    ErrorKind::AlreadyExists => format!("{} already exists", name(to)),
                    ErrorKind::CrossesDevices => "it is on another volume now: move it back by hand".into(),
                    ErrorKind::Unsupported => "this volume can't rename without risking what is in the way".into(),
                    _ => format!("can't move {} to {}: {e}", name(from), to.display()),
                })?;
                self.changed(&[(from.clone(), to.clone())]);
                Ok(Made::default())
            }
            Act::Trash(path) => {
                if !self.unsaved_under(path).is_empty() {
                    return Err(format!("{} has unsaved changes open", name(path)));
                }
                let trashed = self.trash_now(path).map_err(|e| format!("{e:#}"))?;
                Ok(Made { id: trashed.as_ref().map(|t| t.1), at: trashed.map(|t| t.0), fresh: None })
            }
            Act::Create { path, folder } => {
                let dir = path.parent().ok_or("it has no folder")?;
                let file_name = path.file_name().ok_or("it has no name")?.to_string_lossy();
                let made = if *folder { file_ops::create_dir(dir, &file_name) } else { file_ops::create_file(dir, &file_name) };
                made.map_err(|e| format!("{e:#}"))?;
                self.changed(&[]);
                let meta = path.symlink_metadata().ok();
                Ok(Made {
                    id: meta.as_ref().map(Id::of_meta),
                    at: None,
                    fresh: meta.filter(|_| !folder).as_ref().map(Fresh::of),
                })
            }
        }
    }
}

/// What a create or a Trash left, for its step's record.
#[derive(Default)]
struct Made {
    id: Option<Id>,
    at: Option<PathBuf>,
    fresh: Option<Fresh>,
}

/// Record in `op` what doing `act` left. A create or Trash whose entry
/// couldn't be read afterwards stores none, so later checks refuse.
fn note_outcome(op: &mut FileOp, act: &Act, made: Made) {
    match (op, act) {
        (FileOp::Create { id, fresh, .. }, Act::Create { .. }) => (*id, *fresh) = (made.id, made.fresh),
        (FileOp::Trash { at, restored, .. }, Act::Trash(_)) => {
            *at = made.at;
            *restored = false;
        }
        (FileOp::Trash { restored, .. }, _) => *restored = false,
        _ => {}
    }
}

/// Why `what` ("undo", ...) of `op` was refused. A refused restore from the
/// Trash says where the entry is while the Trash holds something there.
fn refusal(what: &str, op: &FileOp, why: String) -> String {
    let mut message = format!("Can't {what} \u{201c}{}\u{201d}: {why}.", op.label());
    if let (true, FileOp::Trash { at: Some(at), .. }) = (what != "redo", op) {
        if at.symlink_metadata().is_ok() {
            message.push_str(&put_back(at));
        }
    }
    message
}

/// Where a Trash refused to give an entry back left it.
fn put_back(at: &Path) -> String {
    format!(" It is in the Trash at {}: Finder's Put Back can restore it.", at.display())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;

    use ghostrealm_core::{SurfaceId, VtabId};

    use ghostrealm_core::Args;

    use super::TOAST_KEY;
    use crate::app_state::{build_registry, AppState};
    use crate::plugins::file_browser::FileBrowserView;
    use crate::plugins::files::FilesView;
    use crate::file_history::Walk;
    use crate::file_jobs::FileJob;
    use crate::file_ops::tests::FakeSystem;
    use crate::plugin::testing::{tmpdir, TmpDir};
    use crate::plugin::undo_panel::{History, StepId};
    use crate::toasts::Level;

    type Calls = Rc<RefCell<Vec<String>>>;

    /// An app with one workspace whose fake Trash is a folder of its own, and
    /// a Files surface in it (`sid`, showing a file outside `d`) to walk from.
    struct Fx {
        // Dropped after the app.
        d: TmpDir,
        trash: TmpDir,
        _other: TmpDir,
        s: AppState,
        calls: Calls,
        vt: VtabId,
        sid: SurfaceId,
    }

    fn fx(tag: &str) -> Fx {
        fx_with(tag, false)
    }

    fn fx_with(tag: &str, fail_trash: bool) -> Fx {
        let d = tmpdir(tag);
        let trash = tmpdir(&format!("{tag}-trash"));
        let other = tmpdir(&format!("{tag}-other"));
        std::fs::write(other.join("o.txt"), "").unwrap();
        let calls = Calls::default();
        let system = FakeSystem { calls: calls.clone(), fail_trash, trash_dir: Some(trash.to_path_buf()) };
        let mut s = AppState::new().with_shell_line("sleep 1").with_system(Box::new(system)).with_unsaved_guard();
        let vt = s.new_empty_vtab();
        let sid = s.open_file_in_new_surface(&other.join("o.txt")).unwrap();
        Fx { d, trash, _other: other, s, calls, vt, sid }
    }

    impl Fx {
        fn go(&mut self, walk: Walk) -> bool {
            self.s.walk_file_history(self.sid, walk)
        }

        fn current(&mut self) -> StepId {
            let h = self.s.file_history(self.vt);
            let id = h.borrow().current();
            id
        }

        /// The level and text of the walk's toast.
        fn toast(&self) -> (Level, String) {
            let e = self.s.toasts().iter().find(|e| e.toast.key.as_deref() == Some(TOAST_KEY)).expect("a toast");
            (e.toast.level, e.toast.message.clone())
        }

        /// Walk, expecting a refusal saying `why` that changes nothing: no
        /// step taken, nothing trashed. Returns the toast's text.
        fn refuses(&mut self, walk: Walk, why: &str) -> String {
            let (before, calls) = (self.current(), self.calls.borrow().len());
            assert!(self.go(walk));
            let (level, message) = self.toast();
            assert!(level == Level::Error && message.contains(why), "{message}");
            assert_eq!(self.current(), before, "the step stays: {message}");
            assert_eq!(self.calls.borrow().len(), calls, "nothing trashed: {message}");
            message
        }

        /// Open `path` in a Files surface of this workspace with an unsaved
        /// change.
        fn edit_unsaved(&mut self, path: &Path) {
            let sid = self.s.open_file_in_new_surface(path).unwrap();
            self.s.editor_mut(sid).unwrap().buffer_mut().insert_str("x");
        }
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    /// Put a new entry with `text` at `path`, as another inode.
    fn replace(path: &Path, text: &str) {
        let tmp = path.with_extension("replacing");
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    #[cfg(unix)]
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut out: Vec<_> =
            std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        out.sort();
        out
    }

    /// The labels of the steps in effect in `vt`'s history, oldest first.
    fn trail(s: &mut AppState, vt: VtabId) -> Vec<String> {
        let h = s.file_history(vt);
        let h = h.borrow();
        let mut out = Vec::new();
        let mut at = Some(h.current());
        while let Some(id) = at {
            out.push(h.label(id));
            at = h.parent(id);
        }
        out.reverse();
        out
    }

    /// Delete `path` and answer the question it asks (outside git, always).
    fn delete(s: &mut AppState, path: &Path) {
        s.delete_path(path).unwrap();
        s.wait_file_jobs();
        if s.confirm().is_some() {
            s.accept_confirm().unwrap();
        }
    }

    fn cut_paste(s: &mut AppState, src: &Path, dir: &Path) {
        s.clip_file(src, true);
        s.paste_into(dir).unwrap();
        s.wait_file_jobs();
    }

    #[test]
    fn operations_are_recorded_in_the_workspace_they_were_made_in() {
        let mut f = fx("undo-scope");
        let (s, d) = (&mut f.s, &f.d);
        s.new_entry(d, "a.md", false).unwrap();
        s.new_entry(d, "src", true).unwrap();
        let b = s.new_empty_vtab();
        s.rename_path(&d.join("a.md"), "b.md").unwrap();
        cut_paste(s, &d.join("b.md"), &d.join("src"));
        delete(s, &d.join("src"));
        assert_eq!(trail(s, f.vt), ["Start", "New file a.md", "New folder src"]);
        assert_eq!(trail(s, b), ["Start", "Rename a.md \u{2192} b.md", "Move b.md \u{2192} src", "Trash src"]);
        assert_eq!(f.calls.borrow().len(), 1);
        assert!(f.trash.join("src/b.md").exists());
    }

    #[test]
    fn a_delete_confirmed_after_switching_workspace_is_recorded_where_it_started() {
        let mut f = fx("undo-scope-delete");
        let (s, d) = (&mut f.s, &f.d);
        std::fs::write(d.join("a.md"), "A").unwrap();
        s.delete_path(&d.join("a.md")).unwrap();
        s.wait_file_jobs();
        assert!(s.confirm().is_some(), "outside git, Delete asks");
        let b = s.new_empty_vtab();
        s.accept_confirm().unwrap();
        assert_eq!(trail(s, f.vt), ["Start", "Trash a.md"]);
        assert_eq!(trail(s, b), ["Start"]);
    }

    #[test]
    fn closing_a_workspace_drops_its_tree_and_a_job_finishing_later_records_nothing() {
        let mut f = fx("undo-scope-close");
        let (s, d) = (&mut f.s, &f.d);
        std::fs::create_dir(d.join("to")).unwrap();
        std::fs::write(d.join("a.md"), "A").unwrap();
        let b = s.new_empty_vtab();
        s.new_entry(d, "n.md", false).unwrap();
        assert!(s.file_histories.contains_key(&b));
        // A copy ahead of it holds the worker, so the paste finishes late.
        s.file_jobs.submit(FileJob::Hold { src: d.join("x"), dir: d.join("y"), until: d.join("go") });
        s.clip_file(&d.join("a.md"), true);
        s.paste_into(&d.join("to")).unwrap();
        s.close_vtab(b);
        assert!(!s.file_histories.contains_key(&b), "the tree went with its workspace");
        std::fs::write(d.join("go"), "").unwrap();
        s.wait_file_jobs();
        assert!(d.join("to/a.md").exists(), "the move itself went ahead");
        assert!(!s.file_histories.contains_key(&b), "and recorded nothing");
        assert_eq!(trail(s, f.vt), ["Start"]);
    }

    #[test]
    fn a_rename_or_paste_that_changes_nothing_records_nothing() {
        let mut f = fx("undo-noop");
        let (s, d) = (&mut f.s, &f.d);
        std::fs::write(d.join("a.md"), "A").unwrap();
        s.rename_path(&d.join("a.md"), "a.md").unwrap();
        cut_paste(s, &d.join("a.md"), d);
        assert!(d.join("a.md").exists());
        assert_eq!(trail(s, f.vt), ["Start"]);
    }

    #[test]
    fn a_create_where_there_is_no_trash_is_not_recorded() {
        let mut f = fx_with("undo-no-trash", true);
        f.s.new_entry(&f.d, "a.md", false).unwrap();
        assert!(f.d.join("a.md").exists());
        assert_eq!(trail(&mut f.s, f.vt), ["Start"]);
    }

    // Undo and redo.

    #[test]
    fn a_new_file_or_folder_undoes_to_the_trash_and_redoes_empty() {
        let mut f = fx("undo-create");
        let d = f.d.to_path_buf();
        f.s.new_entry(&d, "a.md", false).unwrap();
        f.s.new_entry(&d, "src", true).unwrap();
        assert!(f.go(Walk::Undo));
        assert!(!d.join("src").exists() && f.trash.join("src").is_dir());
        assert_eq!(f.toast(), (Level::Info, "Undid new folder src".into()));
        f.go(Walk::Undo);
        assert!(!d.join("a.md").exists() && f.trash.join("a.md").is_file());
        f.go(Walk::Redo);
        f.go(Walk::Redo);
        assert!(d.join("a.md").is_file() && d.join("src").is_dir());
        assert_eq!(f.toast().1, "Redid new folder src");
        f.go(Walk::Undo);
        f.go(Walk::Undo);
        assert!(!d.join("a.md").exists() && !d.join("src").exists(), "the redone entries are the ones checked");
        assert_eq!(trail(&mut f.s, f.vt), ["Start"]);
        assert!(!f.go(Walk::Undo), "nothing before the start");
    }

    #[test]
    fn a_rename_undoes_and_redoes_with_its_open_document_following() {
        let mut f = fx("undo-rename");
        let d = f.d.to_path_buf();
        std::fs::write(d.join("a.md"), "A").unwrap();
        let doc = f.s.open_file_in_new_surface(&d.join("a.md")).unwrap();
        let doc_path = |s: &AppState| s.editor(doc).and_then(|e| e.buffer().path.clone());
        f.s.rename_path(&d.join("a.md"), "b.md").unwrap();
        f.go(Walk::Undo);
        assert_eq!(f.toast().1, "Undid rename a.md \u{2192} b.md");
        assert_eq!((read(&d.join("a.md")), d.join("b.md").exists()), ("A".into(), false));
        assert_eq!(doc_path(&f.s), Some(d.join("a.md")));
        f.go(Walk::Redo);
        assert_eq!((read(&d.join("b.md")), d.join("a.md").exists()), ("A".into(), false));
        assert_eq!(doc_path(&f.s), Some(d.join("b.md")));
        assert!(f.calls.borrow().is_empty());
    }

    #[test]
    fn a_move_undoes_back_to_its_folder_and_redoes() {
        let mut f = fx("undo-move");
        let d = f.d.to_path_buf();
        std::fs::create_dir(d.join("to")).unwrap();
        std::fs::write(d.join("a.md"), "A").unwrap();
        cut_paste(&mut f.s, &d.join("a.md"), &d.join("to"));
        assert_eq!(trail(&mut f.s, f.vt), ["Start", "Move a.md \u{2192} to"]);
        f.go(Walk::Undo);
        assert_eq!((read(&d.join("a.md")), d.join("to/a.md").exists()), ("A".into(), false));
        f.go(Walk::Redo);
        assert_eq!((read(&d.join("to/a.md")), d.join("a.md").exists()), ("A".into(), false));
    }

    #[test]
    fn a_trashed_entry_comes_back_from_where_the_trash_put_it_and_goes_again() {
        let mut f = fx("undo-trash");
        let d = f.d.to_path_buf();
        std::fs::write(d.join("a.md"), "A").unwrap();
        delete(&mut f.s, &d.join("a.md"));
        assert_eq!(read(&f.trash.join("a.md")), "A");
        f.go(Walk::Undo);
        assert_eq!(f.toast().1, "Undid trash a.md");
        assert_eq!((read(&d.join("a.md")), f.trash.join("a.md").exists()), ("A".into(), false));
        f.go(Walk::Redo);
        assert!(!d.join("a.md").exists() && f.trash.join("a.md").exists());
        f.go(Walk::Undo);
        assert_eq!(read(&d.join("a.md")), "A", "the redo's place in the Trash is the one restored");
        assert_eq!(f.calls.borrow().len(), 2, "only Delete and the redo trash");
        assert_eq!(trail(&mut f.s, f.vt), ["Start"], "undo and redo record nothing");
    }

    #[test]
    fn a_walk_to_another_branch_undoes_and_redoes_each_step_between() {
        let mut f = fx("undo-branch");
        let d = f.d.to_path_buf();
        std::fs::write(d.join("a.md"), "A").unwrap();
        f.s.rename_path(&d.join("a.md"), "b.md").unwrap();
        f.go(Walk::Undo);
        f.s.rename_path(&d.join("a.md"), "c.md").unwrap();
        let first = {
            let h = f.s.file_history(f.vt);
            let h = h.borrow();
            h.children(h.root())[0]
        };
        assert!(f.go(Walk::To(first)));
        assert_eq!(names(&d), ["b.md"]);
        assert_eq!(trail(&mut f.s, f.vt), ["Start", "Rename a.md \u{2192} b.md"]);
    }

    #[test]
    fn a_case_only_rename_undoes_and_redoes() {
        let mut f = fx("undo-recase");
        let d = f.d.to_path_buf();
        std::fs::write(d.join("a.md"), "A").unwrap();
        f.s.rename_path(&d.join("a.md"), "A.md").unwrap();
        assert_eq!(names(&d), ["A.md"]);
        f.go(Walk::Undo);
        assert_eq!(names(&d), ["a.md"]);
        f.go(Walk::Redo);
        assert_eq!(names(&d), ["A.md"]);
        assert_eq!(read(&d.join("A.md")), "A");
    }

    #[test]
    fn an_undo_whose_result_is_already_in_place_takes_the_step_without_acting() {
        let mut f = fx("undo-done");
        let d = f.d.to_path_buf();
        f.s.new_entry(&d, "a.md", false).unwrap();
        std::fs::remove_file(d.join("a.md")).unwrap();
        std::fs::write(d.join("b.md"), "B").unwrap();
        delete(&mut f.s, &d.join("b.md"));
        // Put back by hand (Finder's Put Back, say).
        std::fs::rename(f.trash.join("b.md"), d.join("b.md")).unwrap();
        f.go(Walk::Undo);
        f.go(Walk::Undo);
        assert_eq!(trail(&mut f.s, f.vt), ["Start"]);
        assert_eq!(f.calls.borrow().len(), 1, "only the Delete trashed anything");
        assert_eq!(read(&d.join("b.md")), "B");
    }

    // Each check refuses and keeps the data.

    #[test]
    fn undo_of_a_new_file_refuses_once_it_is_written_to() {
        let mut f = fx("guard-written");
        let a = f.s.new_entry(&f.d, "a.md", false).unwrap();
        std::fs::write(&a, "x").unwrap();
        f.refuses(Walk::Undo, "a.md changed since it was created");
        assert_eq!(read(&a), "x");
    }

    #[test]
    fn undo_of_a_new_file_refuses_once_another_entry_took_its_place() {
        let mut f = fx("guard-replaced");
        let a = f.s.new_entry(&f.d, "a.md", false).unwrap();
        replace(&a, "");
        f.refuses(Walk::Undo, "a.md was replaced by another entry");
        assert!(a.is_file());
    }

    #[test]
    fn undo_of_a_new_folder_refuses_once_it_holds_anything() {
        let mut f = fx("guard-nonempty");
        let src = f.s.new_entry(&f.d, "src", true).unwrap();
        std::fs::write(src.join("x"), "X").unwrap();
        f.refuses(Walk::Undo, "src isn't empty");
        assert_eq!(read(&src.join("x")), "X");
    }

    #[cfg(unix)]
    #[test]
    fn undo_of_a_new_folder_refuses_when_it_can_t_be_listed() {
        let mut f = fx("guard-unlistable");
        let src = f.s.new_entry(&f.d, "src", true).unwrap();
        chmod(&src, 0o000);
        f.refuses(Walk::Undo, "can't read");
        chmod(&src, 0o755);
        assert!(src.is_dir());
    }

    #[test]
    fn undo_of_a_new_file_refuses_while_it_has_unsaved_changes_open() {
        let mut f = fx("guard-unsaved");
        let a = f.s.new_entry(&f.d, "a.md", false).unwrap();
        f.edit_unsaved(&a);
        f.refuses(Walk::Undo, "a.md has unsaved changes open");
        assert!(a.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn undo_of_a_new_file_keeps_it_and_the_step_when_the_trash_fails() {
        let mut f = fx("guard-trash-fails");
        let a = f.s.new_entry(&f.d, "a.md", false).unwrap();
        let before = f.current();
        chmod(&f.trash, 0o555);
        f.go(Walk::Undo);
        chmod(&f.trash, 0o755);
        assert_eq!(f.toast().0, Level::Error);
        assert_eq!(f.current(), before);
        assert!(a.is_file() && names(&f.trash).is_empty());
    }

    #[test]
    fn undo_of_a_rename_refuses_when_the_old_name_is_taken() {
        let mut f = fx("guard-p-taken");
        let d = f.d.to_path_buf();
        std::fs::write(d.join("a.md"), "A").unwrap();
        f.s.rename_path(&d.join("a.md"), "b.md").unwrap();
        std::fs::write(d.join("a.md"), "new").unwrap();
        f.refuses(Walk::Undo, "a.md already exists");
        assert_eq!((read(&d.join("a.md")), read(&d.join("b.md"))), ("new".into(), "A".into()));
    }

    #[test]
    fn undo_of_a_rename_refuses_when_another_entry_has_the_new_name() {
        let mut f = fx("guard-q-replaced");
        let d = f.d.to_path_buf();
        std::fs::write(d.join("a.md"), "A").unwrap();
        f.s.rename_path(&d.join("a.md"), "b.md").unwrap();
        replace(&d.join("b.md"), "B2");
        f.refuses(Walk::Undo, "b.md was replaced by another entry");
        assert_eq!(read(&d.join("b.md")), "B2");
        assert!(!d.join("a.md").exists());
    }

    #[test]
    fn undo_of_a_move_refuses_when_its_old_folder_is_gone_or_a_file() {
        let mut f = fx("guard-parent-gone");
        let d = f.d.to_path_buf();
        std::fs::create_dir_all(d.join("x")).unwrap();
        std::fs::create_dir(d.join("to")).unwrap();
        std::fs::write(d.join("x/a.md"), "A").unwrap();
        cut_paste(&mut f.s, &d.join("x/a.md"), &d.join("to"));
        std::fs::remove_dir(d.join("x")).unwrap();
        f.refuses(Walk::Undo, "can't move a.md");
        assert_eq!(read(&d.join("to/a.md")), "A");
        std::fs::write(d.join("x"), "a file").unwrap();
        f.refuses(Walk::Undo, "can't read");
        assert_eq!((read(&d.join("to/a.md")), read(&d.join("x"))), ("A".into(), "a file".into()));
    }

    #[cfg(unix)]
    #[test]
    fn undo_of_a_move_refuses_when_its_old_folder_can_t_be_searched() {
        let mut f = fx("guard-parent-locked");
        let d = f.d.to_path_buf();
        std::fs::create_dir(d.join("x")).unwrap();
        std::fs::create_dir(d.join("to")).unwrap();
        std::fs::write(d.join("x/a.md"), "A").unwrap();
        cut_paste(&mut f.s, &d.join("x/a.md"), &d.join("to"));
        chmod(&d.join("x"), 0o000);
        f.refuses(Walk::Undo, "can't read");
        chmod(&d.join("x"), 0o755);
        assert_eq!(read(&d.join("to/a.md")), "A");
    }

    #[test]
    fn undo_refuses_while_a_file_job_is_running() {
        let mut f = fx("guard-job");
        let d = f.d.to_path_buf();
        std::fs::write(d.join("a.md"), "A").unwrap();
        f.s.rename_path(&d.join("a.md"), "b.md").unwrap();
        f.s.file_jobs.submit(FileJob::Hold { src: d.join("x"), dir: d.join("y"), until: d.join("go") });
        f.refuses(Walk::Undo, "Can't undo while a file operation is running");
        std::fs::write(d.join("go"), "").unwrap();
        f.s.wait_file_jobs();
        assert_eq!(read(&d.join("b.md")), "A");
    }

    #[test]
    fn undo_of_a_trash_refuses_once_the_trash_no_longer_holds_it() {
        let mut f = fx("guard-t-gone");
        let d = f.d.to_path_buf();
        std::fs::write(d.join("a.md"), "A").unwrap();
        delete(&mut f.s, &d.join("a.md"));
        std::fs::remove_file(f.trash.join("a.md")).unwrap();
        let message = f.refuses(Walk::Undo, "a.md is gone");
        assert!(!message.contains("Put Back"), "{message}");
    }

    #[test]
    fn undo_of_a_trash_refuses_when_its_name_is_taken_and_says_where_it_is() {
        let mut f = fx("guard-trash-p-taken");
        let d = f.d.to_path_buf();
        std::fs::write(d.join("a.md"), "A").unwrap();
        delete(&mut f.s, &d.join("a.md"));
        std::fs::write(d.join("a.md"), "new").unwrap();
        let message = f.refuses(Walk::Undo, "a.md already exists");
        let at = f.trash.join("a.md");
        assert!(message.contains("Put Back") && message.contains(&at.display().to_string()), "{message}");
        assert_eq!((read(&d.join("a.md")), read(&at)), ("new".into(), "A".into()));
    }

    #[test]
    fn undo_of_a_trash_refuses_when_another_entry_is_in_its_place_there() {
        let mut f = fx("guard-t-replaced");
        let d = f.d.to_path_buf();
        std::fs::write(d.join("a.md"), "A").unwrap();
        delete(&mut f.s, &d.join("a.md"));
        replace(&f.trash.join("a.md"), "other");
        f.refuses(Walk::Undo, "a.md was replaced by another entry");
        assert_eq!(read(&f.trash.join("a.md")), "other");
        assert!(!d.join("a.md").exists());
    }

    #[test]
    fn redo_of_a_trash_refuses_once_another_entry_took_its_place() {
        let mut f = fx("guard-redo-trash-replaced");
        let d = f.d.to_path_buf();
        std::fs::write(d.join("a.md"), "A").unwrap();
        delete(&mut f.s, &d.join("a.md"));
        f.go(Walk::Undo);
        replace(&d.join("a.md"), "new");
        f.refuses(Walk::Redo, "a.md was replaced by another entry");
        assert_eq!(read(&d.join("a.md")), "new");
    }

    #[test]
    fn redo_of_a_trash_refuses_while_it_has_unsaved_changes_open() {
        let mut f = fx("guard-redo-trash-unsaved");
        let d = f.d.to_path_buf();
        std::fs::write(d.join("a.md"), "A").unwrap();
        delete(&mut f.s, &d.join("a.md"));
        f.go(Walk::Undo);
        f.edit_unsaved(&d.join("a.md"));
        f.refuses(Walk::Redo, "a.md has unsaved changes open");
        assert_eq!(read(&d.join("a.md")), "A");
    }

    #[test]
    fn redo_of_a_new_file_refuses_when_its_name_is_taken() {
        let mut f = fx("guard-redo-create-taken");
        let a = f.s.new_entry(&f.d, "a.md", false).unwrap();
        f.go(Walk::Undo);
        std::fs::write(&a, "X").unwrap();
        f.refuses(Walk::Redo, "a.md already exists");
        assert_eq!(read(&a), "X");
    }

    #[test]
    fn redo_of_a_rename_refuses_when_the_new_name_is_taken() {
        let mut f = fx("guard-redo-rename-taken");
        let d = f.d.to_path_buf();
        std::fs::write(d.join("a.md"), "A").unwrap();
        f.s.rename_path(&d.join("a.md"), "b.md").unwrap();
        f.go(Walk::Undo);
        std::fs::write(d.join("b.md"), "X").unwrap();
        f.refuses(Walk::Redo, "b.md already exists");
        assert_eq!((read(&d.join("a.md")), read(&d.join("b.md"))), ("A".into(), "X".into()));
    }

    #[test]
    fn a_walk_stops_at_the_first_refused_step_keeping_the_steps_before() {
        let mut f = fx("guard-walk-midway");
        let d = f.d.to_path_buf();
        std::fs::write(d.join("a.md"), "A").unwrap();
        f.s.rename_path(&d.join("a.md"), "b.md").unwrap();
        f.s.rename_path(&d.join("b.md"), "c.md").unwrap();
        f.s.new_entry(&d, "n.md", false).unwrap();
        std::fs::write(d.join("a.md"), "blocker").unwrap();
        let root = f.s.file_history(f.vt).borrow().root();
        f.go(Walk::To(root));
        assert_eq!(trail(&mut f.s, f.vt), ["Start", "Rename a.md \u{2192} b.md"]);
        assert_eq!(f.toast().0, Level::Error);
        assert_eq!(names(&d), ["a.md", "b.md"]);
        assert_eq!((read(&d.join("a.md")), read(&d.join("b.md"))), ("blocker".into(), "A".into()));
    }

    #[cfg(unix)]
    #[test]
    fn a_renamed_link_moves_the_link_and_not_what_it_points_to() {
        let mut f = fx("guard-symlink");
        let d = f.d.to_path_buf();
        std::fs::write(d.join("t.md"), "T").unwrap();
        std::os::unix::fs::symlink(d.join("t.md"), d.join("l")).unwrap();
        f.s.rename_path(&d.join("l"), "m").unwrap();
        f.go(Walk::Undo);
        assert_eq!(std::fs::read_link(d.join("l")).unwrap(), d.join("t.md"));
        assert_eq!((names(&d), read(&d.join("t.md"))), (vec!["l".to_string(), "t.md".into()], "T".into()));
        // A file where the link was is another entry.
        std::fs::remove_file(d.join("l")).unwrap();
        std::fs::write(d.join("l"), "file").unwrap();
        f.refuses(Walk::Redo, "l was replaced by another entry");
        assert_eq!(read(&d.join("l")), "file");
    }

    #[test]
    fn a_walk_from_a_surface_in_no_workspace_does_nothing() {
        let mut f = fx("guard-no-workspace");
        let path: PathBuf = f.s.new_entry(&f.d, "a.md", false).unwrap();
        assert!(!f.s.walk_file_history(SurfaceId(u64::MAX), Walk::Undo));
        assert!(path.is_file());
    }

    // The undo panel.

    fn trash_step(f: &mut Fx) -> StepId {
        let h = f.s.file_history(f.vt);
        let h = h.borrow();
        h.children(h.root())[0]
    }

    #[test]
    fn a_trashed_entry_is_restored_on_its_own_leaving_the_steps_after_it() {
        let mut f = fx("undo-restore");
        let d = f.d.to_path_buf();
        std::fs::write(d.join("a.md"), "A").unwrap();
        std::fs::write(d.join("b.md"), "B").unwrap();
        delete(&mut f.s, &d.join("a.md"));
        f.s.rename_path(&d.join("b.md"), "c.md").unwrap();
        let t = trash_step(&mut f);
        assert!(f.go(Walk::Restore(t)));
        assert_eq!(f.toast(), (Level::Info, "Restored a.md".into()));
        assert_eq!(read(&d.join("a.md")), "A");
        let steps = ["Start", "Trash a.md", "Rename b.md \u{2192} c.md"];
        assert_eq!(trail(&mut f.s, f.vt), steps, "the steps stay");
        {
            let h = f.s.file_history(f.vt);
            let h = h.borrow();
            assert_eq!((h.note(t), h.action(t)), (Some("restored"), None));
        }
        assert!(!f.go(Walk::Restore(t)), "once");
        f.go(Walk::Undo);
        f.go(Walk::Undo);
        assert_eq!(trail(&mut f.s, f.vt), ["Start"]);
        assert_eq!(names(&d), ["a.md", "b.md"]);
        f.go(Walk::Redo);
        assert!(!d.join("a.md").exists(), "redo trashes it again");
        assert_eq!(f.s.file_history(f.vt).borrow().action(t), Some("Restore"));
    }

    #[test]
    fn a_restore_makes_an_undo_s_checks() {
        let mut f = fx("guard-restore");
        let d = f.d.to_path_buf();
        std::fs::write(d.join("a.md"), "A").unwrap();
        delete(&mut f.s, &d.join("a.md"));
        std::fs::write(d.join("a.md"), "new").unwrap();
        let t = trash_step(&mut f);
        let message = f.refuses(Walk::Restore(t), "Can't restore \u{201c}Trash a.md\u{201d}: a.md already exists");
        assert!(message.contains("Put Back"), "{message}");
        assert_eq!((read(&d.join("a.md")), read(&f.trash.join("a.md"))), ("new".into(), "A".into()));
        assert_eq!(f.s.file_history(f.vt).borrow().note(t), None);
    }

    fn sidebar(f: &mut Fx) -> &mut FileBrowserView {
        f.s.view_mut(f.sid).and_then(|v| v.downcast_mut::<FilesView>()).unwrap().sidebar_mut()
    }

    #[test]
    fn a_walk_toasts_only_while_the_panel_is_closed() {
        let mut f = fx("undo-quiet");
        let d = f.d.to_path_buf();
        std::fs::write(d.join("a.md"), "A").unwrap();
        f.s.rename_path(&d.join("a.md"), "b.md").unwrap();
        assert!(sidebar(&mut f).toggle_undo_panel(), "the sidebar has the workspace's history");
        f.go(Walk::Undo);
        assert!(d.join("a.md").exists());
        assert!(!f.s.toasts().iter().any(|e| e.toast.key.as_deref() == Some(TOAST_KEY)));
        sidebar(&mut f).toggle_undo_panel();
        f.go(Walk::Redo);
        assert_eq!(f.toast().1, "Redid rename a.md \u{2192} b.md");
    }

    #[test]
    fn the_toggle_command_toggles_the_panel_of_whichever_has_the_keys() {
        let mut f = fx("undo-toggle");
        let toggle = |s: &mut AppState| build_registry().execute("editor.toggle_undo_history", &Args::new(), s).unwrap();
        toggle(&mut f.s);
        assert!(f.s.editor(f.sid).unwrap().undo_panel_open() && !sidebar(&mut f).undo_panel_open());
        f.s.view_mut(f.sid).and_then(|v| v.downcast_mut::<FilesView>()).unwrap().focus_sidebar();
        toggle(&mut f.s);
        assert!(f.s.editor(f.sid).unwrap().undo_panel_open() && sidebar(&mut f).undo_panel_open());
    }
}
