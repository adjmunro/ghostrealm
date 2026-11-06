//! Undo and redo of file operations: each workspace's [`FileHistory`],
//! recorded by the file commands in `files.rs`.
//!
//! [`FileHistory`]: crate::file_history::FileHistory

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::Path;
    use std::rc::Rc;

    use ghostrealm_core::VtabId;

    use crate::app_state::AppState;
    use crate::file_ops::tests::FakeSystem;
    use crate::file_jobs::FileJob;
    use crate::plugin::testing::{tmpdir, TmpDir};
    use crate::plugin::undo_panel::History;

    type Calls = Rc<RefCell<Vec<String>>>;

    /// An app with one workspace whose fake Trash is a folder of its own.
    struct Fx {
        // Dropped after the app.
        d: TmpDir,
        trash: TmpDir,
        s: AppState,
        calls: Calls,
        vt: VtabId,
    }

    fn fx(tag: &str) -> Fx {
        fx_with(tag, false)
    }

    fn fx_with(tag: &str, fail_trash: bool) -> Fx {
        let d = tmpdir(tag);
        let trash = tmpdir(&format!("{tag}-trash"));
        let calls = Calls::default();
        let system = FakeSystem { calls: calls.clone(), fail_trash, trash_dir: Some(trash.to_path_buf()) };
        let mut s = AppState::new().with_shell_line("sleep 1").with_system(Box::new(system));
        let vt = s.new_empty_vtab();
        Fx { d, trash, s, calls, vt }
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
}
