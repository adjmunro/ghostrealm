//! Opening files: every file the app opens shows in a Files surface (see
//! [`AppState::open_file`]).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ghostrealm_core::{PaneId, SurfaceId, VtabId};

use super::{capitalized, AppState};
use crate::plugin::{OpenCx, View};
use crate::plugins::editor;
use crate::plugins::files::{self, FilesView};
use crate::toasts::{Level, Scope, Toast};

/// Opens a view or the user asked for. A refusal (a file that isn't text, a
/// link that isn't the web's) goes to stderr and to an error toast on the
/// pane it was asked from.
impl AppState {
    /// Open `path` as surface `from` asked ([`Self::open_file_from`]; its
    /// error names the file).
    pub fn open_requested_file(&mut self, from: SurfaceId, path: &Path) {
        let scope = self.scope_of(Some(from));
        let opened = self.open_file_from(from, path);
        self.refused(scope, opened);
    }

    /// Show `path` in the sidebar as surface `from` asked
    /// ([`Self::show_in_browser`]).
    pub fn show_requested(&mut self, from: SurfaceId, path: &Path) {
        let scope = self.scope_of(Some(from));
        let shown = self.show_in_browser(path);
        self.refused(scope, shown.with_context(|| format!("couldn't show {} in the sidebar", file_name(path))));
    }

    /// Open `url` as surface `from` asked ([`Self::open_url`]; its error
    /// names the URL).
    pub fn open_requested_url(&mut self, from: SurfaceId, url: &str) {
        let scope = self.scope_of(Some(from));
        let opened = self.open_url(url);
        self.refused(scope, opened);
    }

    /// Open plugin `id` from the "nothing open" screen: in `pane` of
    /// workspace `vt` ([`Self::open_plugin_in_pane`]), else the focused pane.
    pub fn open_requested_plugin(&mut self, at: Option<(VtabId, PaneId)>, id: &str) {
        let title = self.plugins.iter().find(|p| p.id() == id).map_or(id, |p| p.title());
        let (scope, opened) = match at {
            Some((vt, pane)) => (Scope::Pane(pane), self.open_plugin_in_pane(vt, pane, id)),
            None => (self.scope_of(None), self.open_plugin_in_focused(id)),
        };
        self.refused(scope, opened.with_context(|| format!("couldn't open {title}")));
    }

    fn refused(&mut self, scope: Scope, result: Result<()>) {
        if let Err(e) = result {
            eprintln!("ghostrealm: {e:#}");
            self.notify(Toast::new(Level::Error, capitalized(&format!("{e:#}"))).on(scope));
        }
    }

    /// Open `path` from outside any Files surface (the palette, a link,
    /// `file.open`, the agent): in a Files surface of the active workspace
    /// with a tab of it already (that tab shown), else the focused one, else
    /// the one focused last, else a new one as a tab in the focused pane. It
    /// gets a tab of its own there (see [`FilesView::show_doc`]).
    pub fn open_file(&mut self, path: &Path) -> Result<()> {
        if let Some(sid) = self.files_showing(path) {
            self.show_file_in(sid, path);
            return Ok(());
        }
        self.open_file_in(self.files_target(), path)
    }

    /// Open `path` as surface `from` asked (a view's `Request::OpenFile`).
    /// A Files surface's own sidebar opens in that surface, whichever other
    /// shows the file; any other view opens as [`Self::open_file`] does.
    pub fn open_file_from(&mut self, from: SurfaceId, path: &Path) -> Result<()> {
        if !self.is_files(from) {
            return self.open_file(path);
        }
        if self.files_has(from, path) {
            self.show_file_in(from, path);
            return Ok(());
        }
        self.open_file_in(Some(from), path)
    }

    fn open_file_in(&mut self, target: Option<SurfaceId>, path: &Path) -> Result<()> {
        // Made first: a file that won't open changes nothing.
        let doc = self.view_for_path(None, path)?;
        self.place_doc(target, doc, Some(path));
        Ok(())
    }

    /// Open an empty text document in a Files surface, as [`Self::open_file`]
    /// places a file.
    pub fn open_scratch(&mut self) -> Result<()> {
        let plugin = self.plugins.iter().find(|p| p.id() == editor::ID).context("no editor plugin")?;
        let doc = plugin.open(&self.open_cx(None))?;
        self.place_doc(self.files_target(), doc, None);
        Ok(())
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

    /// A Files surface of the active workspace with a tab of `path`, the
    /// focused one first.
    fn files_showing(&self, path: &Path) -> Option<SurfaceId> {
        let focused = self.focused_surface();
        let mut all = self.active().map(|vt| self.surfaces_of(vt)).unwrap_or_default();
        all.sort_by_key(|sid| Some(*sid) != focused);
        all.into_iter().find(|sid| self.files_has(*sid, path))
    }

    /// Whether Files surface `sid` has a tab of `path`.
    pub(super) fn files_has(&self, sid: SurfaceId, path: &Path) -> bool {
        self.views.get(&sid).and_then(|v| v.downcast_ref::<FilesView>()).is_some_and(|f| f.has_path(path))
    }

    /// Bring Files surface `sid` into view, showing its tab of `path`.
    pub(super) fn show_file_in(&mut self, sid: SurfaceId, path: &Path) {
        self.show_surface(sid);
        let Some(files) = self.views.get_mut(&sid).and_then(|v| v.downcast_mut::<FilesView>()) else { return };
        files.show_path(path);
        if let Some(t) = files.title() {
            self.tree.set_surface_title(sid, t, false);
        }
        self.note_focus(sid);
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
    /// `target`, else in a new one.
    pub(super) fn place_doc(&mut self, target: Option<SurfaceId>, doc: Box<dyn View>, path: Option<&Path>) {
        let Some(sid) = target else {
            self.new_files_surface(doc, path);
            return;
        };
        self.show_surface(sid);
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

/// `path`'s last part, to name it in a toast.
fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
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
    fn app(tag: &str) -> (AppState, crate::plugin::testing::TmpDir) {
        let d = tmpdir(tag);
        for n in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(d.join(n), n).unwrap();
        }
        let mut s = AppState::new().with_unsaved_guard();
        s.set_default_dir(Some(d.to_path_buf()));
        s.new_empty_vtab();
        (s, d)
    }

    fn shown(s: &AppState, sid: SurfaceId) -> Option<PathBuf> {
        s.active_doc(sid).and_then(|d| s.doc(d)).and_then(|v| v.path())
    }

    /// The files of surface `sid`'s tabs, in order.
    fn tabs(s: &AppState, sid: SurfaceId) -> Vec<PathBuf> {
        s.view(sid).unwrap().docs().into_iter().filter_map(|d| s.doc((sid, d)).and_then(|v| v.path())).collect()
    }

    fn surfaces(s: &AppState) -> usize {
        s.tree.surface_count()
    }

    fn edit(s: &mut AppState, sid: SurfaceId) {
        assert!(s.views.get_mut(&sid).unwrap().write_input(b"new "));
    }

    #[test]
    fn a_refused_sidebar_open_toasts_on_the_sidebars_pane() {
        let (mut s, d) = app("open-refused");
        std::fs::write(d.join("latin1.txt"), b"caf\xe9").unwrap();
        s.open_file(&d.join("a.txt")).unwrap();
        let sid = s.focused_surface().unwrap();
        let (_, pane) = s.pane_of(sid).unwrap();
        s.split_focused(ghostrealm_core::Axis::LeftRight).unwrap();
        assert_ne!(s.focused_pane().map(|(_, p)| p), Some(pane));
        s.open_requested_file(sid, &d.join("latin1.txt"));
        let toast = &s.toasts().head().expect("a toast").toast;
        assert_eq!((toast.level, toast.scope), (Level::Error, Scope::Pane(pane)));
        assert!(toast.message.starts_with("Can't edit "), "{}", toast.message);
        assert_eq!(toast.message.matches("latin1.txt").count(), 1, "{}", toast.message);
    }

    #[test]
    fn a_link_that_isnt_the_webs_toasts_once_naming_it() {
        let (mut s, d) = app("open-url-refused");
        s.open_file(&d.join("a.txt")).unwrap();
        let sid = s.focused_surface().unwrap();
        s.open_requested_url(sid, "ftp://example.com/x");
        let message = &s.toasts().head().expect("a toast").toast.message;
        assert!(message.starts_with("Not opening ftp://example.com/x: "), "{message}");
        assert_eq!(message.matches("ftp://").count(), 1, "{message}");
    }

    #[test]
    fn a_link_to_an_open_file_by_another_spelling_shows_its_tab() {
        let (mut s, d) = app("open-canonical");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("a.md"), "a").unwrap();
        s.open_file(&d.join("a.md")).unwrap();
        let sid = s.focused_surface().unwrap();
        // What a Preview link to `sub/../a.md` asks for.
        let canon = std::fs::canonicalize(d.join("sub/../a.md")).unwrap();
        s.open_file_from(sid, &canon).unwrap();
        assert_eq!(tabs(&s, sid), [d.join("a.md")], "the open tab, not a second");
    }

    #[test]
    fn a_device_a_folder_or_a_huge_text_file_doesnt_open_and_changes_nothing() {
        let (mut s, d) = app("open-irregular");
        let big = d.join("big.log");
        std::fs::File::create(&big).unwrap().set_len(crate::plugins::editor::buffer::MAX_FILE_BYTES + 1).unwrap();
        #[cfg(unix)]
        assert!(s.open_file(Path::new("/dev/zero")).is_err());
        assert!(s.open_file(&d).is_err());
        assert!(s.open_file(&big).is_err());
        assert_eq!(surfaces(&s), 0, "nothing opened");
        // The cap is the editor's: a large image still opens (as media).
        let png = d.join("big.png");
        std::fs::File::create(&png).unwrap().set_len(crate::plugins::editor::buffer::MAX_FILE_BYTES + 1).unwrap();
        s.open_file(&png).unwrap();
        assert_eq!(surfaces(&s), 1);
    }

    #[test]
    fn a_file_opens_in_the_focused_files_surface_else_the_latest_else_a_new_one() {
        let (mut s, d) = app("open-route");
        // None yet: a new one, in place of the empty pane's picker.
        s.open_file(&d.join("a.txt")).unwrap();
        let first = s.focused_surface().unwrap();
        assert!(s.is_files(first));
        assert_eq!((surfaces(&s), shown(&s, first)), (1, Some(d.join("a.txt"))));
        // Focused: a tab there.
        s.open_file(&d.join("b.txt")).unwrap();
        assert_eq!((surfaces(&s), shown(&s, first)), (1, Some(d.join("b.txt"))));
        assert_eq!(tabs(&s, first), [d.join("a.txt"), d.join("b.txt")]);

        // A second Files surface, focused last; then a terminal takes focus.
        s.open_file_in_new_surface(&d.join("c.txt")).unwrap();
        let second = s.focused_surface().unwrap();
        s.add_empty_surface_in_focused();
        assert!(s.focused_surface().is_some_and(|sid| !s.is_files(sid)));
        s.note_focus(first);
        std::fs::write(d.join("e.txt"), "e").unwrap();
        s.open_file(&d.join("e.txt")).unwrap();
        assert_eq!(s.focused_surface(), Some(first), "the one focused last, brought into view");
        assert_eq!(shown(&s, first), Some(d.join("e.txt")));
        assert_eq!(shown(&s, second), Some(d.join("c.txt")));

        // Another workspace with no Files surface gets its own.
        s.new_empty_vtab();
        s.open_file(&d.join("b.txt")).unwrap();
        let third = s.focused_surface().unwrap();
        assert!(third != first && third != second && s.is_files(third));
    }

    #[test]
    fn opening_a_file_a_tab_shows_shows_that_tab() {
        let (mut s, d) = app("open-same");
        s.open_file(&d.join("a.txt")).unwrap();
        let sid = s.focused_surface().unwrap();
        let a = s.active_doc(sid).unwrap();
        edit(&mut s, sid);
        s.open_file(&d.join("b.txt")).unwrap();
        s.open_file(&d.join("a.txt")).unwrap();
        assert!(s.confirm().is_none(), "nothing replaced, nothing asked");
        assert_eq!(s.active_doc(sid), Some(a));
        assert_eq!(tabs(&s, sid), [d.join("a.txt"), d.join("b.txt")]);
        assert_eq!(s.tree.vtabs()[0].panes()[0].surfaces[0].title, "a.txt", "the pane's tab follows");
    }

    #[test]
    fn a_modified_document_keeps_its_tab_when_another_file_opens() {
        let (mut s, d) = app("open-beside");
        s.open_file(&d.join("a.txt")).unwrap();
        let sid = s.focused_surface().unwrap();
        let a = s.active_doc(sid).unwrap();
        edit(&mut s, sid);
        s.open_file(&d.join("b.txt")).unwrap();
        assert!(s.confirm().is_none());
        assert_eq!(shown(&s, sid), Some(d.join("b.txt")));
        assert!(s.doc(a).is_some_and(|v| v.modified()), "kept, unsaved");
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "a.txt");
        // So does a new buffer.
        build_registry().execute("editor.new", &ghostrealm_core::Args::new(), &mut s).unwrap();
        assert_eq!(s.view(sid).unwrap().docs().len(), 3);
        assert_eq!(shown(&s, sid), None, "the buffer shows");
    }

    #[test]
    fn an_open_from_a_sidebar_stays_in_its_surface() {
        let (mut s, d) = app("open-own-sidebar");
        s.open_file(&d.join("a.txt")).unwrap();
        let first = s.focused_surface().unwrap();
        s.open_file_in_new_surface(&d.join("b.txt")).unwrap();
        let second = s.focused_surface().unwrap();
        // From `second`'s own sidebar, a.txt opens there, though `first` shows it.
        s.open_file_from(second, &d.join("a.txt")).unwrap();
        assert_eq!((s.focused_surface(), shown(&s, second)), (Some(second), Some(d.join("a.txt"))));
        assert_eq!(shown(&s, first), Some(d.join("a.txt")));
        // From elsewhere, the focused surface first: it has a tab of b.txt.
        s.open_file(&d.join("b.txt")).unwrap();
        assert_eq!((s.focused_surface(), shown(&s, second)), (Some(second), Some(d.join("b.txt"))));
        // Only `first` has c.txt: it comes into view.
        s.open_file_from(first, &d.join("c.txt")).unwrap();
        s.show_surface(second);
        s.open_file(&d.join("c.txt")).unwrap();
        assert_eq!((s.focused_surface(), shown(&s, first)), (Some(first), Some(d.join("c.txt"))));
    }

    #[test]
    fn closing_a_document_its_host_no_longer_has_closes_nothing() {
        let (mut s, d) = app("open-stale-close");
        s.open_file(&d.join("a.txt")).unwrap();
        let sid = s.focused_surface().unwrap();
        let a = s.active_doc(sid).unwrap();
        s.open_file(&d.join("b.txt")).unwrap();
        assert!(!s.close_doc(a), "the surface stays");
        assert!(!s.close_doc(a), "a second time, nothing");
        assert_eq!(tabs(&s, sid), [d.join("b.txt")]);
        assert!(!s.close_doc((sid, DocId::SELF)));
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
        assert_eq!(tabs(&s, second), [d.join("b.txt")], "the focused one got no tab");
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
    fn close_closes_tabs_one_at_a_time_asking_about_each_unsaved_one() {
        let (mut s, d) = app("open-close-tabs");
        for n in ["a.txt", "b.txt", "c.txt"] {
            s.open_file(&d.join(n)).unwrap();
        }
        let sid = s.focused_surface().unwrap();
        let docs = s.view(sid).unwrap().docs();
        assert!(s.doc_mut((sid, docs[0])).unwrap().write_input(b"a "));
        assert!(s.doc_mut((sid, docs[2])).unwrap().write_input(b"c "));

        s.close_focused_surface();
        let c = s.confirm().expect("c.txt is unsaved");
        assert!(c.message.contains("c.txt") && !c.message.contains("a.txt"), "{}", c.message);
        s.discard_confirm().unwrap();
        assert_eq!(tabs(&s, sid), [d.join("a.txt"), d.join("b.txt")]);
        assert_eq!(shown(&s, sid), Some(d.join("b.txt")), "its left neighbour");
        assert_eq!(s.tree.vtabs()[0].panes()[0].surfaces[0].title, "b.txt");

        s.close_focused_surface();
        assert!(s.confirm().is_none(), "b.txt has no changes");
        s.close_focused_surface();
        assert!(s.confirm().is_some_and(|c| c.message.contains("a.txt")));
        assert_eq!(s.accept_confirm().unwrap(), vec![d.join("a.txt")], "Save and Close");
        assert!(s.active_doc(sid).is_none() && s.view(sid).is_some(), "the sidebar stays");
        s.close_focused_surface();
        assert!(s.view(sid).is_none());
    }

    #[test]
    fn a_tabs_close_button_closes_it_asking_first() {
        let (mut s, d) = app("open-close-button");
        s.open_file(&d.join("a.txt")).unwrap();
        s.open_file(&d.join("b.txt")).unwrap();
        let sid = s.focused_surface().unwrap();
        let docs = s.view(sid).unwrap().docs();
        assert!(s.doc_mut((sid, docs[0])).unwrap().write_input(b"a "));
        s.close_doc_of(sid, docs[0]);
        assert!(s.confirm().is_some_and(|c| c.message.contains("a.txt")), "the tab not shown asks too");
        s.cancel_confirm();
        assert_eq!(s.view(sid).unwrap().docs(), docs);
        s.close_doc_of(sid, docs[0]);
        s.discard_confirm().unwrap();
        assert_eq!(tabs(&s, sid), [d.join("b.txt")]);
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "a.txt");
    }

    #[test]
    fn quit_asks_about_every_unsaved_tab_and_save_acts_on_the_shown_one() {
        let (mut s, d) = app("open-quit-tabs");
        s.open_file(&d.join("a.txt")).unwrap();
        s.open_file(&d.join("b.txt")).unwrap();
        let sid = s.focused_surface().unwrap();
        let docs = s.view(sid).unwrap().docs();
        assert!(s.doc_mut((sid, docs[0])).unwrap().write_input(b"a "));
        assert!(s.doc_mut((sid, docs[1])).unwrap().write_input(b"b "));
        // Cmd+S and Save As are the shown b.txt's.
        assert!(s.send_key_to_focused(&cmd(Key::Char('s'))));
        assert_eq!(std::fs::read_to_string(d.join("b.txt")).unwrap(), "b b.txt\n");
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "a.txt");
        s.open_save_as(sid);
        assert_eq!(s.picker_purpose(), Some(super::super::Pick::SaveAs((sid, docs[1]))));
        s.close_picker();

        assert!(s.doc_mut((sid, docs[1])).unwrap().write_input(b"b "));
        s.request_quit();
        let c = s.confirm().expect("asks");
        assert!(c.message.starts_with("2 buffers have unsaved changes"), "{}", c.message);
        assert_eq!(s.accept_confirm().unwrap(), vec![d.join("a.txt"), d.join("b.txt")]);
        assert!(s.quitting());
    }

    #[test]
    fn checking_files_on_disk_reaches_tabs_not_shown() {
        let (mut s, d) = app("open-check-tabs");
        s.open_file(&d.join("a.txt")).unwrap();
        s.open_file(&d.join("b.txt")).unwrap();
        let sid = s.focused_surface().unwrap();
        let a = s.view(sid).unwrap().docs()[0];
        std::fs::write(d.join("a.txt"), "theirs").unwrap();
        assert!(s.check_open_files());
        let editor = s.doc((sid, a)).and_then(|v| v.downcast_ref::<editor::EditorView>()).unwrap();
        assert_eq!(editor.buffer().lines, ["theirs"]);
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
        assert!(!files.is_preview(files.active_doc()), "a key opens a kept tab");
        build_registry().execute("files.focus_sidebar", &ghostrealm_core::Args::new(), &mut s).unwrap();
        let files = s.view(sid).and_then(|v| v.downcast_ref::<FilesView>()).unwrap();
        assert!(files.sidebar_has_keys());
        // Right on it again: its tab shows, with the keyboard.
        s.send_key_to_focused(&crate::plugin::testing::press(Key::Right));
        let files = s.view(sid).and_then(|v| v.downcast_ref::<FilesView>()).unwrap();
        assert!(!files.sidebar_has_keys());
        assert_eq!(tabs(&s, sid), [d.join("a.txt")]);
    }

    #[test]
    fn next_and_previous_tab_commands_cycle_the_focused_surfaces_tabs() {
        let (mut s, d) = app("open-cycle");
        s.open_file(&d.join("a.txt")).unwrap();
        s.open_file(&d.join("b.txt")).unwrap();
        let sid = s.focused_surface().unwrap();
        let mut r = build_registry();
        r.execute("files.next_tab", &ghostrealm_core::Args::new(), &mut s).unwrap();
        assert_eq!(shown(&s, sid), Some(d.join("a.txt")));
        r.execute("files.prev_tab", &ghostrealm_core::Args::new(), &mut s).unwrap();
        assert_eq!(shown(&s, sid), Some(d.join("b.txt")));
    }
}
