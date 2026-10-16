//! Opening files: every file the app opens shows in a Files surface (see
//! [`AppState::open_file`]).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ghostrealm_core::{SurfaceId, VtabId};

use super::{AppState, Confirm, Confirmed, DocRef};
use crate::plugin::{OpenCx, View};
use crate::plugins::editor;
use crate::plugins::files::{self, FilesView};

impl AppState {
    /// Open `path` in a Files surface: one of the active workspace showing
    /// it already (only brought into view), else the focused one, else the
    /// one focused last, else a new one as a tab in the focused pane. It
    /// replaces the document shown (see [`Self::place_doc`]).
    pub fn open_file(&mut self, path: &Path) -> Result<()> {
        if let Some(sid) = self.files_showing(path) {
            self.show_surface(sid);
            return Ok(());
        }
        let target = self.files_target();
        // Made first: a file that won't open changes nothing.
        let doc = self.view_for_path(None, path)?;
        self.place_doc(target, doc, Some(path.to_path_buf()));
        Ok(())
    }

    /// Open an empty text document in a Files surface, as [`Self::open_file`]
    /// places a file.
    pub fn open_scratch(&mut self) -> Result<()> {
        let doc = self.scratch_view()?;
        self.place_doc(self.files_target(), doc, None);
        Ok(())
    }

    fn scratch_view(&self) -> Result<Box<dyn View>> {
        let plugin = self.plugins.iter().find(|p| p.id() == editor::ID).context("no editor plugin")?;
        plugin.open(&self.open_cx(None))
    }

    /// What a plugin opens a view with, starting in `cwd`.
    pub(super) fn open_cx(&self, cwd: Option<PathBuf>) -> OpenCx {
        OpenCx {
            cwd,
            command: self.shell_line.clone(),
            waker: self.waker.clone(),
            languages: self.languages.clone(),
            files: self.files_session.clone(),
        }
    }

    /// The Files surface a file opens in: the focused surface when it is
    /// one, else the active workspace's focused last.
    pub(super) fn files_target(&self) -> Option<SurfaceId> {
        let focused = self.focused_surface().filter(|sid| self.is_files(*sid));
        focused.or_else(|| {
            let vt = self.active()?;
            self.surfaces_of(vt).into_iter().filter(|sid| self.is_files(*sid)).max_by_key(|sid| self.files_seen.get(sid))
        })
    }

    fn is_files(&self, sid: SurfaceId) -> bool {
        self.views.get(&sid).is_some_and(|v| v.downcast_ref::<FilesView>().is_some())
    }

    /// A Files surface of the active workspace showing `path`, the focused
    /// one first.
    fn files_showing(&self, path: &Path) -> Option<SurfaceId> {
        let focused = self.focused_surface();
        let mut all = self.active().map(|vt| self.surfaces_of(vt)).unwrap_or_default();
        all.sort_by_key(|sid| Some(*sid) != focused);
        all.into_iter().find(|sid| self.is_files(*sid) && self.files_shows(*sid, path))
    }

    /// Whether Files surface `sid` shows `path`.
    pub(super) fn files_shows(&self, sid: SurfaceId, path: &Path) -> bool {
        let shown = self.active_doc(sid).and_then(|d| self.doc(d)).and_then(|v| v.path());
        shown.is_some_and(|p| crate::languages::same_file(&p, path))
    }

    /// Every surface of workspace `vt`, in tree order.
    fn surfaces_of(&self, vt: VtabId) -> Vec<SurfaceId> {
        let Some(v) = self.tree.vtab(vt) else { return Vec::new() };
        v.panes().into_iter().flat_map(|p| p.surfaces.iter().map(|s| s.id)).collect()
    }

    /// Note that surface `sid` took the keyboard: the latest Files surface
    /// focused is where a file opens when none is focused.
    pub fn note_focus(&mut self, sid: SurfaceId) {
        if self.is_files(sid) {
            self.focus_stamp += 1;
            self.files_seen.insert(sid, self.focus_stamp);
        }
    }

    /// Show `doc` (of `path`, `None` for a scratch buffer) in Files surface
    /// `target`, else in a new one. A document there with unsaved changes
    /// autosaves when it autosaves; otherwise the user is asked first (and
    /// it is replaced once answered), and with nobody to ask (the agent
    /// channel) `doc` opens in a new Files surface instead, so no edit is
    /// lost.
    pub(super) fn place_doc(&mut self, target: Option<SurfaceId>, doc: Box<dyn View>, path: Option<PathBuf>) {
        let Some(sid) = target else {
            self.new_files_surface(doc, path.as_deref());
            return;
        };
        let modified = |s: &Self| s.active_doc(sid).filter(|d| s.doc(*d).is_some_and(|v| v.modified()));
        if let Some((_, id)) = modified(self) {
            let cfg = &self.cfg;
            if let Some(v) = self.views.get_mut(&sid).and_then(|v| v.document_mut(id)) {
                v.autosave(cfg);
            }
        }
        match modified(self) {
            None => {
                self.show_surface(sid);
                self.replace_doc(sid, doc);
            }
            Some(old) if self.guard_unsaved => {
                self.show_surface(sid);
                self.ask_replace(old, path);
            }
            Some(_) => self.new_files_surface(doc, path.as_deref()),
        }
    }

    /// Ask whether to save `old`'s unsaved changes before `path` (`None`: an
    /// empty buffer) replaces it. A buffer with no file offers Save As.
    fn ask_replace(&mut self, old: DocRef, path: Option<PathBuf>) {
        let (message, save, can_save) = self.unsaved_question(&[old]);
        let (message, accept) = match can_save {
            true => (message, "Save and Open"),
            false => {
                let name = self.doc(old).and_then(|v| v.title()).unwrap_or_default();
                (format!("\u{201c}{name}\u{201d} has unsaved changes."), "Save As\u{2026}")
            }
        };
        self.confirm = Some(Confirm {
            message,
            accept: Some(accept),
            discard: Some("Open Without Saving"),
            then: Confirmed::UnsavedOpen { doc: old, path, save },
            late: None,
        });
    }

    /// Replace the document Files surface `sid` shows with a new one of
    /// `path` (`None`: an empty buffer), once its old one was saved or given
    /// up. Nothing happens when the surface has gone.
    pub(super) fn replace_with(&mut self, sid: SurfaceId, path: Option<&Path>) -> Result<()> {
        if !self.is_files(sid) {
            return Ok(());
        }
        let doc = match path {
            Some(p) => self.view_for_path(None, p)?,
            None => self.scratch_view()?,
        };
        self.replace_doc(sid, doc);
        Ok(())
    }

    fn replace_doc(&mut self, sid: SurfaceId, doc: Box<dyn View>) {
        let Some(files) = self.views.get_mut(&sid).and_then(|v| v.downcast_mut::<FilesView>()) else { return };
        files.show_doc(doc);
        if let Some(t) = files.title() {
            self.tree.set_surface_title(sid, t, false);
        }
        self.note_focus(sid);
    }

    /// A new Files surface showing `doc`, as a tab in the focused pane (or
    /// in place of the "nothing open" picker there). Its sidebar starts at
    /// the workspace's directory when the file is inside it, else at the
    /// file's folder.
    fn new_files_surface(&mut self, doc: Box<dyn View>, path: Option<&Path>) {
        let Some((vt, pane)) = self.focused_pane() else { return };
        let dir = self.resolve_cwd(vt);
        let root = match (path, dir) {
            (Some(p), Some(d)) if p.starts_with(&d) => Some(d),
            (Some(p), _) => p.parent().map(Path::to_path_buf),
            (None, d) => d,
        };
        let cx = self.open_cx(None);
        let mut files = FilesView::new(root.unwrap_or_else(|| files::start_dir(&cx)), &cx);
        files.show_doc(doc);
        let target = match self.focused_surface() {
            Some(sid) if self.surface_is_empty(sid) => Some(sid),
            _ => self.tree.add_surface(vt, pane),
        };
        if let Some(sid) = target {
            self.insert_view(sid, Box::new(files));
            self.note_focus(sid);
        }
    }

    /// Bring surface `sid` into view and give it the keyboard: its
    /// workspace's focused pane, that pane's active tab.
    pub(super) fn show_surface(&mut self, sid: SurfaceId) {
        let Some(vt) = self.vtab_of_surface(sid) else { return };
        if self.active() != Some(vt) {
            self.focus_vtab(vt);
        }
        let Some(v) = self.tree.vtab_mut(vt) else { return };
        let found = v.panes().into_iter().find_map(|p| Some((p.id, p.surfaces.iter().position(|s| s.id == sid)?)));
        if let Some((pane, index)) = found {
            v.focused_pane = pane;
            if let Some(p) = v.pane_mut(pane) {
                p.active = index;
            }
        }
    }

    /// Open `path` in a new Files surface of its own, whatever is focused:
    /// for tests holding several files open at once. Returns the surface.
    #[cfg(test)]
    pub fn open_file_in_new_surface(&mut self, path: &Path) -> Result<SurfaceId> {
        let doc = self.view_for_path(None, path)?;
        self.new_files_surface(doc, Some(path));
        self.focused_surface().context("no pane to open in")
    }

    /// The editor document surface `sid` shows, if it shows one.
    #[cfg(test)]
    pub fn editor(&self, sid: SurfaceId) -> Option<&editor::EditorView> {
        self.active_doc(sid).and_then(|d| self.doc(d)).and_then(|v| v.downcast_ref())
    }

    /// [`Self::editor`], mutably.
    #[cfg(test)]
    pub fn editor_mut(&mut self, sid: SurfaceId) -> Option<&mut editor::EditorView> {
        let doc = self.active_doc(sid)?;
        self.doc_mut(doc).and_then(|v| v.downcast_mut())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::build_registry;
    use crate::plugin::testing::{cmd, tmpdir};
    use crate::plugin::DocId;
    use ghostrealm_terminal::Key;

    /// A guarded state with one empty workspace on a folder of `a.txt`,
    /// `b.txt`, and `c.txt` (each holding its name).
    fn app(tag: &str) -> (AppState, PathBuf) {
        let d = tmpdir(tag);
        for n in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(d.join(n), n).unwrap();
        }
        let mut s = AppState::new().with_unsaved_guard();
        s.set_config(no_autosave());
        s.set_default_dir(Some(d.clone()));
        s.new_empty_vtab();
        (s, d)
    }

    /// A config whose editors never save by themselves.
    fn no_autosave() -> ghostrealm_core::Config {
        let mut cfg = ghostrealm_core::Config::default();
        (cfg.editor.autosave_on_unfocus, cfg.editor.autosave_after) = (false, 0);
        cfg
    }

    fn shown(s: &AppState, sid: SurfaceId) -> Option<PathBuf> {
        s.active_doc(sid).and_then(|d| s.doc(d)).and_then(|v| v.path())
    }

    fn surfaces(s: &AppState) -> usize {
        s.tree.surface_count()
    }

    #[test]
    fn a_file_opens_in_the_focused_files_surface_else_the_latest_else_a_new_one() {
        let (mut s, d) = app("open-route");
        // None yet: a new one, in place of the empty pane's picker.
        s.open_file(&d.join("a.txt")).unwrap();
        let first = s.focused_surface().unwrap();
        assert!(s.is_files(first));
        assert_eq!((surfaces(&s), shown(&s, first)), (1, Some(d.join("a.txt"))));
        // Focused: it replaces the document there.
        s.open_file(&d.join("b.txt")).unwrap();
        assert_eq!((surfaces(&s), shown(&s, first)), (1, Some(d.join("b.txt"))));

        // A second Files surface, focused last; then a terminal takes focus.
        s.open_file_in_new_surface(&d.join("c.txt")).unwrap();
        let second = s.focused_surface().unwrap();
        s.add_empty_surface_in_focused();
        assert!(s.focused_surface().is_some_and(|sid| !s.is_files(sid)));
        s.note_focus(first);
        s.open_file(&d.join("a.txt")).unwrap();
        assert_eq!(s.focused_surface(), Some(first), "the one focused last, brought into view");
        assert_eq!(shown(&s, first), Some(d.join("a.txt")));
        assert_eq!(shown(&s, second), Some(d.join("c.txt")));

        // Another workspace with no Files surface gets its own.
        s.new_empty_vtab();
        s.open_file(&d.join("b.txt")).unwrap();
        let third = s.focused_surface().unwrap();
        assert!(third != first && third != second && s.is_files(third));
    }

    #[test]
    fn opening_the_file_shown_already_keeps_its_document() {
        let (mut s, d) = app("open-same");
        s.open_file(&d.join("a.txt")).unwrap();
        let sid = s.focused_surface().unwrap();
        let doc = s.active_doc(sid).unwrap();
        assert!(s.views.get_mut(&sid).unwrap().write_input(b"x"));
        s.open_file(&d.join("a.txt")).unwrap();
        assert!(s.confirm().is_none(), "nothing replaced, nothing asked");
        assert_eq!(s.active_doc(sid), Some(doc));
    }

    #[test]
    fn replacing_a_modified_document_asks_and_each_button_decides() {
        let (mut s, d) = app("open-replace");
        s.open_file(&d.join("a.txt")).unwrap();
        let sid = s.focused_surface().unwrap();
        let old = s.active_doc(sid).unwrap();
        assert!(s.views.get_mut(&sid).unwrap().write_input(b"new "));

        // Cancel: the edited document stays.
        s.open_file(&d.join("b.txt")).unwrap();
        let c = s.confirm().expect("asks about the unsaved changes");
        assert_eq!((c.accept, c.discard), (Some("Save and Open"), Some("Open Without Saving")));
        assert!(c.message.contains("a.txt"), "{}", c.message);
        s.cancel_confirm();
        assert_eq!(s.active_doc(sid), Some(old));

        // Save and open: written, then replaced, under a new id.
        s.open_file(&d.join("b.txt")).unwrap();
        assert_eq!(s.accept_confirm().unwrap(), vec![d.join("a.txt")]);
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "new a.txt\n");
        assert_eq!(shown(&s, sid), Some(d.join("b.txt")));
        assert_ne!(s.active_doc(sid), Some(old));
        let title = s.tree.vtabs()[0].panes()[0].surfaces[0].title.clone();
        assert_eq!(title, "b.txt", "the tab follows");

        // Open without saving: replaced, the file untouched.
        assert!(s.views.get_mut(&sid).unwrap().write_input(b"lost "));
        s.open_file(&d.join("c.txt")).unwrap();
        s.discard_confirm().unwrap();
        assert_eq!(shown(&s, sid), Some(d.join("c.txt")));
        assert_eq!(std::fs::read_to_string(d.join("b.txt")).unwrap(), "b.txt");

        // An empty buffer is saved where the picker says, then replaced.
        build_registry().execute("editor.new", &ghostrealm_core::Args::new(), &mut s).unwrap();
        assert!(s.confirm().is_none(), "c.txt was unmodified");
        let scratch = s.active_doc(sid).unwrap();
        assert!(s.views.get_mut(&sid).unwrap().write_input(b"scratch"));
        s.open_file(&d.join("a.txt")).unwrap();
        assert_eq!(s.confirm().map(|c| c.accept), Some(Some("Save As\u{2026}")));
        assert_eq!(s.accept_confirm().unwrap(), Vec::<PathBuf>::new());
        assert_eq!(s.picker_purpose(), Some(super::super::Pick::SaveAs(scratch)));
        s.confirm_picker_with(d.join("kept.txt")).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("kept.txt")).unwrap(), "scratch\n");
        assert_eq!(shown(&s, sid), Some(d.join("a.txt")), "then the file opened");
        // A Save As cancelled opens nothing.
        build_registry().execute("editor.new", &ghostrealm_core::Args::new(), &mut s).unwrap();
        assert!(s.views.get_mut(&sid).unwrap().write_input(b"more"));
        s.open_file(&d.join("b.txt")).unwrap();
        s.accept_confirm().unwrap();
        s.close_picker();
        assert_eq!(shown(&s, sid), None, "still the buffer");
    }

    #[test]
    fn save_and_open_over_a_file_changed_on_disk_asks_and_then_opens() {
        let (mut s, d) = app("open-conflict");
        s.open_file(&d.join("a.txt")).unwrap();
        let sid = s.focused_surface().unwrap();
        assert!(s.views.get_mut(&sid).unwrap().write_input(b"mine "));
        std::fs::write(d.join("a.txt"), "theirs").unwrap();
        s.open_file(&d.join("b.txt")).unwrap();
        s.accept_confirm().unwrap();
        let c = s.confirm().expect("the changed file asks before it is written");
        assert_eq!(c.accept, Some("Overwrite"));
        assert_eq!(shown(&s, sid), Some(d.join("a.txt")), "nothing replaced yet");
        assert_eq!(s.accept_confirm().unwrap(), vec![d.join("a.txt")]);
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "mine a.txt\n");
        assert_eq!(shown(&s, sid), Some(d.join("b.txt")));
    }

    #[test]
    fn a_file_deleted_before_the_answer_opens_as_a_new_one() {
        let (mut s, d) = app("open-gone");
        s.open_file(&d.join("a.txt")).unwrap();
        let sid = s.focused_surface().unwrap();
        assert!(s.views.get_mut(&sid).unwrap().write_input(b"x"));
        s.open_file(&d.join("b.txt")).unwrap();
        std::fs::remove_file(d.join("b.txt")).unwrap();
        s.discard_confirm().unwrap();
        assert_eq!(shown(&s, sid), Some(d.join("b.txt")));
        assert_eq!(s.editor(sid).map(|e| e.buffer().lines.clone()), Some(vec![String::new()]));
    }

    #[test]
    fn an_autosaving_document_saves_instead_of_asking() {
        let (mut s, d) = app("open-autosave");
        s.set_config(ghostrealm_core::Config::default());
        s.open_file(&d.join("a.txt")).unwrap();
        let sid = s.focused_surface().unwrap();
        assert!(s.views.get_mut(&sid).unwrap().write_input(b"auto "));
        s.open_file(&d.join("b.txt")).unwrap();
        assert!(s.confirm().is_none());
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "auto a.txt\n");
        assert_eq!(shown(&s, sid), Some(d.join("b.txt")));
    }

    #[test]
    fn a_file_shown_anywhere_in_the_workspace_comes_into_view() {
        let (mut s, d) = app("open-anywhere");
        s.open_file(&d.join("a.txt")).unwrap();
        let first = s.focused_surface().unwrap();
        s.open_file_in_new_surface(&d.join("b.txt")).unwrap();
        let second = s.focused_surface().unwrap();
        s.open_file(&d.join("a.txt")).unwrap();
        assert_eq!(s.focused_surface(), Some(first));
        assert_eq!(shown(&s, second), Some(d.join("b.txt")), "the focused one kept its file");
    }

    #[test]
    fn close_save_and_save_as_act_on_the_hosted_document() {
        let (mut s, d) = app("open-docs");
        s.open_file(&d.join("a.txt")).unwrap();
        let sid = s.focused_surface().unwrap();
        let doc = s.active_doc(sid).unwrap();
        assert_ne!(doc.1, DocId::SELF);

        // Cmd+S writes the file.
        assert!(s.views.get_mut(&sid).unwrap().write_input(b"1 "));
        assert!(s.send_key_to_focused(&cmd(Key::Char('s'))));
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "1 a.txt\n");

        // Save As asks about this document and retitles the tab.
        s.open_save_as(sid);
        assert_eq!(s.picker_purpose(), Some(super::super::Pick::SaveAs(doc)));
        s.confirm_picker_with(d.join("renamed.txt")).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("renamed.txt")).unwrap(), "1 a.txt\n");
        assert_eq!(s.tree.vtabs()[0].panes()[0].surfaces[0].title, "renamed.txt");

        // Close (Cmd+W) asks about the unsaved document, then closes it,
        // leaving the sidebar; closing again closes the surface.
        assert!(s.views.get_mut(&sid).unwrap().write_input(b"2 "));
        s.close_focused_surface();
        assert!(s.confirm().is_some_and(|c| c.message.contains("renamed.txt")));
        s.discard_confirm().unwrap();
        assert!(s.active_doc(sid).is_none() && s.view(sid).is_some());
        assert_eq!(s.tree.vtabs()[0].panes()[0].surfaces[0].title, "Files");
        s.close_focused_surface();
        assert!(s.view(sid).is_none());
    }

    #[test]
    fn a_key_in_the_sidebar_opens_the_file_there() {
        let (mut s, d) = app("open-key");
        build_registry().execute("file_browser.new", &ghostrealm_core::Args::new(), &mut s).unwrap();
        let sid = s.focused_surface().unwrap();
        assert!(s.is_files(sid) && s.active_doc(sid).is_none(), "only the sidebar");
        s.send_key_to_focused(&crate::plugin::testing::press(Key::Down));
        s.send_key_to_focused(&crate::plugin::testing::press(Key::Right));
        assert_eq!(shown(&s, sid), Some(d.join("a.txt")));
        let files = s.view(sid).and_then(|v| v.downcast_ref::<FilesView>()).unwrap();
        assert!(!files.sidebar_has_keys(), "the keyboard moved to the document");
        build_registry().execute("files.focus_sidebar", &ghostrealm_core::Args::new(), &mut s).unwrap();
        let files = s.view(sid).and_then(|v| v.downcast_ref::<FilesView>()).unwrap();
        assert!(files.sidebar_has_keys());
    }
}
