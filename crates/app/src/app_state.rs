//! The live application state: the workspace [`Tree`] plus the plugin views
//! (terminals, editors, file browsers, ...) that fill its surfaces, and the
//! command registry built over it.
//!
//! This is the single source of truth the GUI, the palette, and the agent
//! channel all act on. It is `!Send` (views own VT engines) and lives on the UI
//! thread. Views size themselves when the GUI paints them; headless, a terminal
//! keeps its default grid.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use ghostrealm_core::{
    ArgKind, ArgSpec, Axis, CloseOnExit, CmdError, CmdOutcome, CommandMeta, Inbox, PaneId, Registry, SurfaceId,
    TabStatus, Tree, VtabId,
};
use ghostrealm_core::Config;
use ghostrealm_terminal::{KeyPress, Lifecycle};

use crate::file_jobs::FileJobs;
use crate::file_ops::{HostSystem, System};
use crate::languages::same_file;
use crate::plugin::{EventCx, OpenCx, Plugin, Request, UiMetrics, View};
use crate::plugins::editor;
use crate::plugins::file_browser::{FileBrowserView, Mode};
use crate::plugins::terminal::{self, TerminalView};

mod files;
pub use files::{FileClip, FileReport};

/// Name of the dedicated workspace the settings file opens in (shown italic).
pub const SETTINGS_VTAB_NAME: &str = "settings";

/// Reserved surface id addressing the floating picker's file browser
/// (e.g. as a button owner). The picker is not a tree surface, so it never
/// renders as a pane and survives prunes.
pub const PICKER_SID: SurfaceId = SurfaceId(u64::MAX);

/// After Enter, a vtab is shown busy for at least this long even if the shell's
/// foreground process group hasn't moved yet — the command may not have forked
/// (or produced output) before the next pump. Bridges that race for silent jobs.
const OPTIMISTIC_BUSY_GRACE: Duration = Duration::from_millis(600);

/// What confirming the floating picker does with its choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Pin the active workspace's root to a directory.
    ActiveRoot,
    /// Create a new workspace rooted at a directory.
    NewWorkspace,
    /// Open a file in the focused pane with this plugin.
    OpenWith(&'static str),
    /// Save this surface's view to a file ([`View::save_as`]).
    SaveAs(SurfaceId),
    /// Save a copy of this surface's view to a file ([`View::save_copy`]).
    SaveCopy(SurfaceId),
}

impl Pick {
    /// What the picker's browser lists and lets the user choose.
    fn mode(self) -> Mode {
        match self {
            Pick::ActiveRoot | Pick::NewWorkspace => Mode::PickDir,
            Pick::OpenWith(_) => Mode::PickFile,
            Pick::SaveAs(_) | Pick::SaveCopy(_) => Mode::Save,
        }
    }
}

/// A question the user answers before an action goes ahead (e.g. replacing a
/// file). While one is open it is the only thing that takes input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirm {
    pub message: String,
    /// The accept button's label: it names the action ("Replace"). `None` when
    /// the question has no safe way to go ahead, leaving only Cancel and
    /// `discard`.
    pub accept: Option<&'static str>,
    /// A third button that goes ahead without doing what `accept` does
    /// ("Quit Without Saving").
    pub discard: Option<&'static str>,
    then: Confirmed,
    /// When it appeared, for a question a background job opened (Delete's
    /// git check) rather than the user's own key or click.
    late: Option<Instant>,
}

/// How long a question a background job opened ignores keys: an Enter typed
/// for something else (a command in a terminal) must not answer it.
pub const LATE_CONFIRM_GRACE: Duration = Duration::from_millis(400);

impl Confirm {
    /// Whether a key at `now` may answer it: not within
    /// [`LATE_CONFIRM_GRACE`] of a background job opening it.
    pub fn takes_keys_at(&self, now: Instant) -> bool {
        self.late.is_none_or(|at| now >= at + LATE_CONFIRM_GRACE)
    }

    /// Whether Enter cancels instead of accepting (and Cancel is drawn as the
    /// primary button): accepting would write over another program's version
    /// of a file, or cut a running copy off half done, which a stray Enter
    /// must not do.
    pub fn enter_cancels(&self) -> bool {
        matches!(self.then, Confirmed::DiskConflict { .. } | Confirmed::BusyQuit)
    }
}

/// What accepting a [`Confirm`] does.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Confirmed {
    /// Finish the picker with this path.
    Pick(std::path::PathBuf),
    /// Leave, after saving `save` (accept) or without saving (discard).
    Unsaved { what: Leave, save: Vec<SurfaceId> },
    /// Move this path to the Trash.
    Trash(std::path::PathBuf),
    /// Move this path to the Trash, after saving `save` (accept) or without
    /// saving (discard) the open views of it.
    UnsavedTrash { path: std::path::PathBuf, save: Vec<SurfaceId> },
    /// `sid`'s file changed on disk since it was loaded: write over it
    /// (accept) or load the disk version over its edits (discard). Then carry
    /// on with `rest`, the other views a Save and Close was saving, and `then`.
    DiskConflict { sid: SurfaceId, rest: Vec<SurfaceId>, then: Option<Resume> },
    /// A copy or move is running: quit now anyway, leaving it half done.
    BusyQuit,
}

/// What a save that stopped to ask about a changed file goes on to do.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Resume {
    /// Leave, once the saving is done.
    Leave(Leave),
    /// Move this path to the Trash, once the saving is done.
    Trash(std::path::PathBuf),
}

/// What leaving gives up: part of the tree, or the whole app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Leave {
    /// The app exits.
    Quit,
    /// One surface (tab) in a pane.
    Surface(VtabId, PaneId, SurfaceId),
    /// A pane and every surface in it.
    Pane(VtabId, PaneId),
    /// A workspace and everything in it.
    Vtab(VtabId),
}

pub struct AppState {
    pub tree: Tree,
    /// Each surface's content. A surface without a view is "empty" — it shows
    /// the open-something picker.
    views: HashMap<SurfaceId, Box<dyn View>>,
    /// Registered content kinds, in picker order.
    plugins: Vec<Box<dyn Plugin>>,
    /// The floating picker, while open, and what confirming it does.
    picker: Option<(FileBrowserView, Pick)>,
    /// The open confirmation, if any.
    confirm: Option<Confirm>,
    /// Whether closing or quitting asks first when a view has unsaved changes.
    /// The GUI turns it on; the agent channel and headless runs close at once,
    /// since nobody is there to answer.
    guard_unsaved: bool,
    /// An accepted quit is waiting for the window to exit.
    quitting: bool,
    /// Optional command line new terminals run (`sh -c <line>`); `None` = the
    /// user's shell.
    shell_line: Option<String>,
    /// Shared source for per-surface PTY wakers (the GUI wires this to its event
    /// loop). Cloned into a fresh `PtyWaker` for each spawned terminal.
    waker: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Language services handed to views opening files.
    languages: Option<Arc<crate::languages::Languages>>,
    next_tab_number: u32,
    /// Per-vtab deadline until which an optimistic (post-Enter) busy status holds
    /// even if the foreground process group hasn't moved yet.
    optimistic_busy: HashMap<VtabId, Instant>,
    /// Inbox timing config (auto-read dwell + unfocus grace).
    inbox: Inbox,
    /// Base directory new workspaces start in when they have no pinned root.
    default_dir: Option<std::path::PathBuf>,
    /// When a terminal closes by itself after its shell exits.
    close_on_exit: CloseOnExit,
    /// When the active vtab was focused, for the auto-read dwell timer.
    focus_started: Instant,
    /// The vtab most recently auto-read by dwell, and when — for the unfocus grace
    /// (leaving too soon after an auto-read reverts it to unread).
    last_auto_read: Option<(VtabId, Instant)>,
    /// The Trash, the file manager, and the default apps.
    system: Box<dyn System>,
    /// The file Cut or Copy picked, for Paste.
    file_clip: Option<FileClip>,
    /// Text a command wants on the system clipboard, until the window takes it.
    clipboard_out: Option<String>,
    /// The worker running file commands' copies, moves, and git checks.
    file_jobs: FileJobs,
    /// How long a file command waits for its job before leaving it to the
    /// worker ([`files::INLINE_WAIT`]).
    file_job_wait: Duration,
    /// Outcomes of file jobs that finished after their command returned,
    /// until the window (or the agent channel) takes them.
    file_reports: Vec<FileReport>,
}

impl AppState {
    pub fn new() -> Self {
        AppState {
            tree: Tree::new(),
            views: HashMap::new(),
            plugins: crate::plugins::builtin(),
            picker: None,
            confirm: None,
            guard_unsaved: false,
            quitting: false,
            shell_line: None,
            waker: None,
            languages: None,
            next_tab_number: 1,
            optimistic_busy: HashMap::new(),
            inbox: Inbox::default(),
            default_dir: None,
            close_on_exit: CloseOnExit::default(),
            focus_started: Instant::now(),
            last_auto_read: None,
            system: Box::new(HostSystem),
            file_clip: None,
            clipboard_out: None,
            file_jobs: FileJobs::default(),
            file_job_wait: files::INLINE_WAIT,
            file_reports: Vec::new(),
        }
    }

    /// Set the inbox timing config (auto-read dwell + unfocus grace).
    pub fn set_inbox_config(&mut self, inbox: Inbox) {
        self.inbox = inbox;
    }

    /// Set when a terminal closes by itself after its shell exits (from config).
    pub fn set_close_on_exit(&mut self, mode: CloseOnExit) {
        self.close_on_exit = mode;
    }

    /// Set the base directory new workspaces start in (from config).
    pub fn set_default_dir(&mut self, dir: Option<std::path::PathBuf>) {
        self.default_dir = dir;
    }

    /// The directory associated with `vt` for its sidebar label: the workspace's
    /// pinned root, else the app default (else `None`).
    pub fn vtab_dir(&self, vt: VtabId) -> Option<std::path::PathBuf> {
        self.tree
            .vtab(vt)
            .and_then(|v| v.root_dir.clone())
            .or_else(|| self.default_dir.clone())
    }

    /// The directory a new surface in `vt` should start in: the workspace's pinned
    /// root, else the app default (else the shell's default when `None`).
    fn resolve_cwd(&self, vt: VtabId) -> Option<std::path::PathBuf> {
        self.tree
            .vtab(vt)
            .and_then(|v| v.root_dir.clone())
            .or_else(|| self.default_dir.clone())
    }

    /// Pin the active workspace's root directory (new terminals/editors start here).
    pub fn set_active_root_dir(&mut self, dir: impl Into<std::path::PathBuf>) {
        if let Some(vt) = self.active() {
            if let Some(v) = self.tree.vtab_mut(vt) {
                v.root_dir = Some(dir.into());
            }
        }
    }

    /// Use `sh -c <line>` for spawned surfaces instead of the login shell.
    pub fn with_shell_line(mut self, line: impl Into<String>) -> Self {
        self.shell_line = Some(line.into());
        self
    }

    /// Wake `f` whenever any surface produces output (the GUI passes a closure
    /// that pokes its event loop). Terminals spawned after this call use it.
    pub fn with_waker(mut self, f: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.file_jobs.set_waker(Some(f.clone()));
        self.waker = Some(f);
        self
    }

    /// Give views that open files these language services.
    pub fn with_languages(mut self, languages: Arc<crate::languages::Languages>) -> Self {
        self.languages = Some(languages);
        self
    }

    /// Create a new vtab with a spawned terminal; it becomes active.
    pub fn new_vtab(&mut self) -> Result<VtabId> {
        let name = format!("tab {}", self.next_tab_number);
        self.next_tab_number += 1;
        let (vt, _pane, surf) = self.tree.add_vtab(name);
        self.materialize_plugin(surf, vt, terminal::ID)?;
        Ok(vt)
    }

    /// Create a new empty vtab (no terminal) — it opens on the "nothing open"
    /// screen where the user picks what to open. Becomes active.
    pub fn new_empty_vtab(&mut self) -> VtabId {
        let name = format!("tab {}", self.next_tab_number);
        self.next_tab_number += 1;
        let (vt, _pane) = self.tree.add_empty_vtab(name);
        vt
    }

    /// Create a new vtab and run `command_line` in it ("Run Anything"): a normal
    /// interactive shell (like a new terminal), with the command typed in and
    /// submitted — so the session stays live and interactive afterwards. The vtab
    /// is named after the command until the program sets its own title; it becomes
    /// active.
    pub fn new_vtab_running(&mut self, command_line: impl Into<String>) -> Result<VtabId> {
        let line = command_line.into();
        let name = line.split_whitespace().next().unwrap_or("run").to_string();
        self.next_tab_number += 1;
        let (vt, _pane, surf) = self.tree.add_vtab(name);
        self.materialize_plugin(surf, vt, terminal::ID)?; // an interactive shell, not `sh -c`
        // Feed the command to the live shell (it echoes + runs it, then stays
        // interactive). The PTY buffers this until the shell is ready to read.
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        self.write_input(surf, &bytes);
        Ok(vt)
    }

    fn active(&self) -> Option<VtabId> {
        self.tree.active_vtab()
    }

    fn focused_pane(&self) -> Option<(VtabId, ghostrealm_core::tree::PaneId)> {
        let vt = self.active()?;
        let v = self.tree.vtab(vt)?;
        Some((vt, v.focused_pane))
    }

    pub fn split_focused(&mut self, axis: Axis) -> Result<()> {
        let Some((vt, pane)) = self.focused_pane() else {
            return Ok(());
        };
        // A new split opens empty (the "nothing open" picker), not a terminal.
        self.tree.split_empty(vt, pane, axis);
        Ok(())
    }

    pub fn new_surface_in_focused(&mut self) -> Result<()> {
        let Some((vt, pane)) = self.focused_pane() else {
            return Ok(());
        };
        if let Some(surf) = self.tree.add_surface(vt, pane) {
            self.materialize_plugin(surf, vt, terminal::ID)?;
        }
        Ok(())
    }

    /// Add `view` as a new tab in the focused pane and make it active.
    pub fn open_view_in_focused(&mut self, view: Box<dyn View>) {
        let Some((vt, pane)) = self.focused_pane() else {
            return;
        };
        if let Some(sid) = self.tree.add_surface(vt, pane) {
            self.insert_view(sid, view);
        }
    }

    /// Add an empty surface (a new tab with no content) to the focused pane and
    /// make it active. It shows the open-something picker until a kind is chosen.
    pub fn add_empty_surface_in_focused(&mut self) {
        if let Some((vt, pane)) = self.focused_pane() {
            self.tree.add_surface(vt, pane);
        }
    }

    /// Give an existing (empty) surface a fresh view from plugin `id`.
    fn materialize_plugin(&mut self, sid: SurfaceId, vt: VtabId, id: &str) -> Result<()> {
        let cwd = self.resolve_cwd(vt);
        self.materialize_plugin_at(sid, id, cwd)
    }

    /// [`Self::materialize_plugin`], starting in `cwd`.
    fn materialize_plugin_at(&mut self, sid: SurfaceId, id: &str, cwd: Option<std::path::PathBuf>) -> Result<()> {
        let cx = OpenCx {
            cwd,
            command: self.shell_line.clone(),
            waker: self.waker.clone(),
            languages: self.languages.clone(),
        };
        let plugin = self
            .plugins
            .iter()
            .find(|p| p.id() == id)
            .ok_or_else(|| anyhow::anyhow!("no plugin `{id}`"))?;
        let view = plugin.open(&cx)?;
        self.insert_view(sid, view);
        Ok(())
    }

    /// Install `view` as surface `sid`'s content, titling its tab.
    fn insert_view(&mut self, sid: SurfaceId, view: Box<dyn View>) {
        if let Some(t) = view.title() {
            self.tree.set_surface_title(sid, t, false);
        }
        self.views.insert(sid, view);
    }

    /// Open plugin `id` in the focused pane: fill the active empty slot in place
    /// when there is one (the picker), otherwise add a new tab.
    pub fn open_plugin_in_focused(&mut self, id: &str) -> Result<()> {
        self.open_plugin_in_focused_at(id, None)
    }

    /// [`Self::open_plugin_in_focused`], starting in `cwd` (else the
    /// workspace's directory).
    pub fn open_plugin_in_focused_at(&mut self, id: &str, cwd: Option<std::path::PathBuf>) -> Result<()> {
        let Some((vt, pane)) = self.focused_pane() else {
            return Ok(());
        };
        let target = match self.focused_surface() {
            Some(sid) if self.surface_is_empty(sid) => Some(sid),
            _ => self.tree.add_surface(vt, pane),
        };
        if let Some(sid) = target {
            let cwd = cwd.or_else(|| self.resolve_cwd(vt));
            self.materialize_plugin_at(sid, id, cwd)?;
        }
        Ok(())
    }

    /// Open plugin `id` in pane `pane` of workspace `vt`, focusing that pane
    /// first so the result lands there and not in the previously focused pane.
    /// Does nothing when `vt` is no longer the active workspace (the press that
    /// armed the button may predate a workspace switch).
    pub fn open_plugin_in_pane(
        &mut self,
        vt: VtabId,
        pane: ghostrealm_core::tree::PaneId,
        id: &str,
    ) -> Result<()> {
        if self.active() != Some(vt) {
            return Ok(());
        }
        let Some(v) = self.tree.vtab_mut(vt) else {
            return Ok(());
        };
        if v.pane_mut(pane).is_none() {
            return Ok(());
        }
        v.focused_pane = pane;
        self.open_plugin_in_focused(id)
    }

    /// Registered plugins, in picker order.
    pub fn plugins(&self) -> &[Box<dyn Plugin>] {
        &self.plugins
    }

    /// Surface `id`'s plugin view, if it has one.
    pub fn view(&self, id: SurfaceId) -> Option<&dyn View> {
        self.views.get(&id).map(|v| v.as_ref())
    }

    pub fn view_mut(&mut self, id: SurfaceId) -> Option<&mut (dyn View + 'static)> {
        self.views.get_mut(&id).map(|v| v.as_mut())
    }

    /// Every surface's view, in no particular order.
    pub fn views(&self) -> impl Iterator<Item = (SurfaceId, &dyn View)> {
        self.views.iter().map(|(sid, v)| (*sid, v.as_ref()))
    }

    /// Whether `id` is an empty surface (no content chosen yet).
    pub fn surface_is_empty(&self, id: SurfaceId) -> bool {
        !self.views.contains_key(&id)
    }

    /// Whether the focused pane shows the "nothing open" picker (an empty pane, or
    /// an empty-kind surface tab).
    pub fn focused_shows_picker(&self) -> bool {
        match self.focused_surface() {
            None => self.focused_pane().is_some(),
            Some(sid) => self.surface_is_empty(sid),
        }
    }

    /// Open `path` in a new tab in the focused pane, with the first plugin that
    /// claims it (the editor claims anything).
    pub fn open_path_in_focused(&mut self, path: &Path) -> Result<()> {
        let view = self.view_for_path(None, path)?;
        self.open_view_in_focused(view);
        Ok(())
    }

    /// Open `path` in a new tab in the focused pane with plugin `id`.
    pub fn open_path_with_in_focused(&mut self, id: &str, path: &Path) -> Result<()> {
        let view = self.view_for_path(Some(id), path)?;
        self.open_view_in_focused(view);
        Ok(())
    }

    /// A view of `path` from plugin `id`, or else from the first plugin that
    /// claims the path.
    fn view_for_path(&self, id: Option<&str>, path: &Path) -> Result<Box<dyn View>> {
        let plugin = self
            .plugins
            .iter()
            .find(|p| match id {
                Some(id) => p.id() == id,
                None => p.opens_path(path),
            })
            .ok_or_else(|| anyhow::anyhow!("no plugin opens {}", path.display()))?;
        let cx = OpenCx {
            cwd: self.active().and_then(|vt| self.resolve_cwd(vt)),
            command: None,
            waker: self.waker.clone(),
            languages: self.languages.clone(),
        };
        plugin.open_path(path, &cx)
    }

    /// Open `path` in a new split beside the focused pane (e.g. beside the file
    /// browser), falling back to a tab if the split can't be made.
    pub fn open_path_split(&mut self, path: &Path, axis: Axis) -> Result<()> {
        let view = self.view_for_path(None, path)?;
        if let Some((vt, pane)) = self.focused_pane() {
            self.tree.split_empty(vt, pane, axis);
        }
        self.open_view_in_focused(view);
        Ok(())
    }

    /// Whether the floating picker is open.
    pub fn picker_open(&self) -> bool {
        self.picker.is_some()
    }

    /// The floating picker's file browser, while open.
    pub fn picker(&self) -> Option<&FileBrowserView> {
        self.picker.as_ref().map(|(b, _)| b)
    }

    /// The floating picker's file browser, while open.
    pub fn picker_mut(&mut self) -> Option<&mut FileBrowserView> {
        self.picker.as_mut().map(|(b, _)| b)
    }

    /// What confirming the open picker does.
    pub fn picker_purpose(&self) -> Option<Pick> {
        self.picker.as_ref().map(|(_, p)| *p)
    }

    /// Open the floating picker for `purpose`. A new workspace starts at the
    /// app default directory; every other purpose starts at the active
    /// workspace's dir (else the app default, else the current dir).
    pub fn open_picker(&mut self, purpose: Pick) {
        let root = match purpose {
            Pick::NewWorkspace => self.default_root(),
            _ => self.picker_root(),
        };
        let browser = FileBrowserView::picker(root, purpose.mode()).with_waker(self.waker.clone());
        self.picker = Some((browser, purpose));
    }

    /// Where the picker starts: the active workspace's dir, else the app
    /// default, else the current dir.
    fn picker_root(&self) -> std::path::PathBuf {
        self.active()
            .and_then(|vt| self.resolve_cwd(vt))
            .unwrap_or_else(|| self.default_root())
    }

    /// The app default directory, else the current dir.
    fn default_root(&self) -> std::path::PathBuf {
        self.default_dir
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."))
    }

    /// Ask where to save surface `sid`'s view: the picker, beside the view's
    /// file and named after it when it has one.
    pub fn open_save_as(&mut self, sid: SurfaceId) {
        self.open_save_dialog(sid, Pick::SaveAs(sid));
    }

    /// Ask where to save a copy of surface `sid`'s view ([`Self::open_save_as`]
    /// without re-pointing the view).
    pub fn open_save_copy(&mut self, sid: SurfaceId) {
        self.open_save_dialog(sid, Pick::SaveCopy(sid));
    }

    fn open_save_dialog(&mut self, sid: SurfaceId, purpose: Pick) {
        let file = self.view(sid).and_then(|v| v.path());
        let root = file
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.picker_root());
        let mut browser = FileBrowserView::picker(root, Mode::Save).with_waker(self.waker.clone());
        if let Some(name) = file.as_deref().and_then(Path::file_name) {
            browser.set_name(name.to_string_lossy());
        }
        self.picker = Some((browser, purpose));
    }

    /// Close the floating picker.
    pub fn close_picker(&mut self) {
        self.picker = None;
        self.confirm = None;
    }

    /// The open confirmation, if any.
    pub fn confirm(&self) -> Option<&Confirm> {
        self.confirm.as_ref()
    }

    /// Dismiss the confirmation; what asked for it stays as it was.
    pub fn cancel_confirm(&mut self) {
        self.confirm = None;
    }

    /// Answer the confirmation as Enter does: accept, or cancel when
    /// [`Confirm::enter_cancels`]. Returns the files written.
    pub fn enter_confirm(&mut self) -> Result<Vec<std::path::PathBuf>> {
        if self.confirm.as_ref().is_some_and(Confirm::enter_cancels) {
            self.cancel_confirm();
            return Ok(Vec::new());
        }
        self.accept_confirm()
    }

    /// Go ahead with the confirmed action. Returns the files written. A
    /// question with no accept button stays open.
    pub fn accept_confirm(&mut self) -> Result<Vec<std::path::PathBuf>> {
        if self.confirm.as_ref().is_some_and(|c| c.accept.is_none()) {
            return Ok(Vec::new());
        }
        match self.confirm.take() {
            Some(Confirm { then: Confirmed::Trash(path), .. }) => {
                self.trash_now(&path)?;
                Ok(Vec::new())
            }
            Some(Confirm { then: Confirmed::UnsavedTrash { path, save }, .. }) => {
                self.save_views(&save, Some(Resume::Trash(path)))
            }
            Some(Confirm { then: Confirmed::DiskConflict { sid, rest, then }, .. }) => {
                let view = self
                    .views
                    .get_mut(&sid)
                    .ok_or_else(|| anyhow::anyhow!("the view to save has closed"))?;
                let mut written = Vec::new();
                if let Some(path) = view.path() {
                    view.save_as(path.clone())?;
                    written.push(path);
                }
                written.extend(self.save_views(&rest, then)?);
                Ok(written)
            }
            Some(Confirm { then: Confirmed::Pick(path), .. }) => {
                Ok(self.finish_picker(path)?.into_iter().collect())
            }
            Some(Confirm { then: Confirmed::Unsaved { what, save }, .. }) => {
                // Nothing is closed unless every save worked.
                self.save_views(&save, Some(Resume::Leave(what)))
            }
            Some(Confirm { then: Confirmed::BusyQuit, .. }) => {
                self.quitting = true;
                Ok(Vec::new())
            }
            None => Ok(Vec::new()),
        }
    }

    /// Save each of `surfaces` to its own file, then do `then`. A view whose
    /// file changed on disk stops the saving to ask ([`Self::ask_disk_conflict`]),
    /// which carries on with the rest once answered. Returns the files written.
    fn save_views(&mut self, surfaces: &[SurfaceId], then: Option<Resume>) -> Result<Vec<std::path::PathBuf>> {
        let mut written = Vec::new();
        for (i, sid) in surfaces.iter().enumerate() {
            let view = self
                .views
                .get_mut(sid)
                .ok_or_else(|| anyhow::anyhow!("the view to save has closed"))?;
            let Some(path) = view.path() else { continue };
            if view.disk_conflict() {
                self.ask_disk_conflict_then(*sid, surfaces[i + 1..].to_vec(), then);
                return Ok(written);
            }
            view.save_as(path.clone())?;
            written.push(path);
        }
        match then {
            Some(Resume::Leave(what)) => self.perform(what),
            Some(Resume::Trash(path)) => self.trash_now(&path)?,
            None => {}
        }
        Ok(written)
    }

    /// Ask whether to write over `sid`'s file, which changed on disk since the
    /// view loaded or wrote it (Overwrite), load the disk version instead
    /// (Reload, losing the view's edits), or leave things as they are.
    pub fn ask_disk_conflict(&mut self, sid: SurfaceId) {
        self.ask_disk_conflict_then(sid, Vec::new(), None);
    }

    fn ask_disk_conflict_then(&mut self, sid: SurfaceId, rest: Vec<SurfaceId>, then: Option<Resume>) {
        let name = self.views.get(&sid).and_then(|v| v.title()).unwrap_or_default();
        self.confirm = Some(Confirm {
            message: format!("\u{201c}{name}\u{201d} changed on disk since you opened it."),
            accept: Some("Overwrite"),
            discard: Some("Reload"),
            then: Confirmed::DiskConflict { sid, rest, then },
            late: None,
        });
    }

    /// Go ahead with the confirmed action without its saving (for a changed
    /// file, by loading the disk version). Does nothing unless the question
    /// has a discard button. Returns the files written.
    pub fn discard_confirm(&mut self) -> Result<Vec<std::path::PathBuf>> {
        if !self.confirm.as_ref().is_some_and(|c| c.discard.is_some()) {
            return Ok(Vec::new());
        }
        match self.confirm.take() {
            Some(Confirm { then: Confirmed::Unsaved { what, .. }, .. }) => self.perform(what),
            Some(Confirm { then: Confirmed::UnsavedTrash { path, .. }, .. }) => self.trash_now(&path)?,
            // The reloaded text is new to the user: the close or quit that
            // asked stops, and so does the saving of the other views.
            Some(Confirm { then: Confirmed::DiskConflict { sid, .. }, .. }) => {
                self.views
                    .get_mut(&sid)
                    .ok_or_else(|| anyhow::anyhow!("the view to reload has closed"))?
                    .reload()?;
            }
            _ => {}
        }
        Ok(Vec::new())
    }

    /// Ask before closing or quitting over unsaved changes ([`Self::confirm`]).
    pub fn with_unsaved_guard(mut self) -> Self {
        self.guard_unsaved = true;
        self
    }

    /// Leave the app, asking first when views have unsaved changes.
    /// [`Self::quitting`] says when it is time to exit.
    pub fn request_quit(&mut self) {
        self.leave(Leave::Quit);
    }

    /// Whether quitting now would ask about unsaved changes first.
    pub fn has_unsaved(&self) -> bool {
        self.guard_unsaved && !self.unsaved_in(Leave::Quit).is_empty()
    }

    /// Whether a quit has been agreed and the window should exit.
    pub fn quitting(&self) -> bool {
        self.quitting
    }


    /// Confirm the picker with its browser's choice, then close it. With no
    /// choice yet (a file picker with nothing selected), it stays open.
    /// Returns the file written, if any.
    pub fn confirm_picker(&mut self) -> Result<Option<std::path::PathBuf>> {
        match self.picker.as_ref().and_then(|(b, _)| b.choice()) {
            Some(path) => self.confirm_picker_with(path),
            None => Ok(None),
        }
    }

    /// Close the picker and act on `path` per its purpose. Saving over another
    /// existing file asks first ([`Self::confirm`]) and leaves the picker open.
    /// Returns the file written, if any.
    pub fn confirm_picker_with(
        &mut self,
        path: std::path::PathBuf,
    ) -> Result<Option<std::path::PathBuf>> {
        if let Some((_, Pick::SaveAs(sid) | Pick::SaveCopy(sid))) = &self.picker {
            let own = self.view(*sid).and_then(|v| v.path());
            let copy = matches!(self.picker, Some((_, Pick::SaveCopy(_))));
            let stale = !copy && self.view(*sid).is_some_and(|v| v.disk_conflict());
            if path.is_file() && (copy || stale || own.as_deref() != Some(path.as_path())) {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                self.confirm = Some(Confirm {
                    message: format!("\u{201c}{name}\u{201d} already exists. Replace it?"),
                    accept: Some("Replace"),
                    discard: None,
                    then: Confirmed::Pick(path),
                    late: None,
                });
                return Ok(None);
            }
        }
        self.finish_picker(path)
    }

    /// Close the picker and act on `path` per its purpose.
    fn finish_picker(&mut self, path: std::path::PathBuf) -> Result<Option<std::path::PathBuf>> {
        let Some((_, purpose)) = self.picker.take() else {
            return Ok(None);
        };
        match purpose {
            Pick::ActiveRoot => self.set_active_root_dir(path),
            Pick::NewWorkspace => {
                self.new_workspace_in(path);
            }
            Pick::OpenWith(id) => self.open_path_with_in_focused(id, &path)?,
            Pick::SaveAs(sid) => {
                let view = self
                    .views
                    .get_mut(&sid)
                    .ok_or_else(|| anyhow::anyhow!("the view to save has closed"))?;
                view.save_as(path.clone())?;
                if let Some(t) = view.title() {
                    self.tree.set_surface_title(sid, t, false);
                }
                return Ok(Some(path));
            }
            Pick::SaveCopy(sid) => {
                let view = self
                    .views
                    .get_mut(&sid)
                    .ok_or_else(|| anyhow::anyhow!("the view to save has closed"))?;
                view.save_copy(path.clone())?;
                self.changed(&[]);
                return Ok(Some(path));
            }
        }
        Ok(None)
    }

    /// Create a new empty workspace rooted at `dir`; it becomes active.
    pub fn new_workspace_in(&mut self, dir: impl Into<std::path::PathBuf>) -> VtabId {
        let vt = self.new_empty_vtab();
        self.set_active_root_dir(dir);
        vt
    }

    /// Create a new workspace named `name` (pinned) whose sole surface is `view`
    /// (no shell spawned). Returns the new vtab id.
    pub fn new_view_vtab(&mut self, name: impl Into<String>, view: Box<dyn View>) -> VtabId {
        let (vt, _pane, surf) = self.tree.add_vtab(name);
        if let Some(v) = self.tree.vtab_mut(vt) {
            v.user_named = true;
        }
        self.insert_view(surf, view);
        vt
    }

    /// An editor on the settings file, with its language service.
    fn settings_view(&self, path: &Path) -> Result<Box<dyn View>> {
        self.view_for_path(Some(editor::ID), path)
    }

    /// Open the settings file in a dedicated "settings" workspace: focus the
    /// existing one (opening the editor there if it was closed), or create it.
    pub fn open_settings(&mut self, path: std::path::PathBuf) -> Result<()> {
        let existing = self
            .tree
            .vtabs()
            .iter()
            .find(|v| v.name == SETTINGS_VTAB_NAME)
            .map(|v| v.id);
        match existing {
            Some(vt) => {
                let has_editor = self
                    .tree
                    .vtab(vt)
                    .map(|v| {
                        v.panes()
                            .into_iter()
                            .flat_map(|p| p.surfaces.iter())
                            .any(|s| self.view(s.id).is_some_and(|v| v.plugin() == editor::ID))
                    })
                    .unwrap_or(false);
                // Make the editor first: a file that won't open leaves the
                // focus where it was.
                let view = if has_editor { None } else { Some(self.settings_view(&path)?) };
                self.focus_vtab(vt);
                if let Some(view) = view {
                    self.open_view_in_focused(view);
                }
            }
            None => {
                let view = self.settings_view(&path)?;
                let vt = self.new_view_vtab(SETTINGS_VTAB_NAME, view);
                self.focus_vtab(vt);
            }
        }
        Ok(())
    }

    /// Persist every view's unsaved state that has a home (autosave on app
    /// blur/quit). Returns whether the config file was among what was saved (so
    /// the caller can hot-reload it).
    pub fn autosave_all(&mut self, cfg: &Config) -> bool {
        self.autosave_all_with_config(cfg, ghostrealm_core::config::config_path().as_deref())
    }

    /// [`Self::autosave_all`], with the config file at `config_path`.
    fn autosave_all_with_config(&mut self, cfg: &Config, config_path: Option<&Path>) -> bool {
        let mut saved_config = false;
        for view in self.views.values_mut() {
            if let Some(path) = view.autosave(cfg) {
                saved_config |= config_path.is_some_and(|c| same_file(c, &path));
            }
        }
        saved_config
    }

    /// Cycle focus to the next (`+1`) or previous (`-1`) pane in the active vtab.
    fn cycle_pane(&mut self, step: isize) {
        let Some(vt) = self.active() else { return };
        let Some(v) = self.tree.vtab(vt) else { return };
        let panes: Vec<_> = v.panes().iter().map(|p| p.id).collect();
        let n = panes.len();
        if n < 2 {
            return;
        }
        let cur = v.focused_pane;
        let pos = panes.iter().position(|&p| p == cur).unwrap_or(0) as isize;
        let next = panes[(pos + step).rem_euclid(n as isize) as usize];
        if let Some(v) = self.tree.vtab_mut(vt) {
            v.focused_pane = next;
        }
    }

    /// Cycle focus to the next pane in the active vtab.
    pub fn focus_next_pane(&mut self) {
        self.cycle_pane(1);
    }

    /// Cycle focus to the previous pane in the active vtab.
    pub fn focus_prev_pane(&mut self) {
        self.cycle_pane(-1);
    }

    pub fn close_focused_pane(&mut self) {
        if let Some((vt, pane)) = self.focused_pane() {
            self.leave(Leave::Pane(vt, pane));
        }
    }

    /// Close the active surface of the focused pane. Closing the last surface of a
    /// split collapses the pane; closing the last surface of the sole pane leaves
    /// the workspace on its empty screen. On an empty pane, closing collapses the
    /// split (when it's one of several) or, if it's the sole pane, closes the
    /// workspace.
    pub fn close_focused_surface(&mut self) {
        let Some((vt, pane)) = self.focused_pane() else {
            return;
        };
        if let Some(sid) = self.focused_surface() {
            self.leave(Leave::Surface(vt, pane, sid));
            return;
        }
        // Empty pane: collapse the split if there are siblings, else close the vtab.
        let sole = self.tree.vtab(vt).map(|v| v.panes().len() <= 1).unwrap_or(true);
        self.leave(if sole { Leave::Vtab(vt) } else { Leave::Pane(vt, pane) });
    }

    pub fn close_active_vtab(&mut self) {
        if let Some(vt) = self.active() {
            self.leave(Leave::Vtab(vt));
        }
    }

    /// Close a specific vtab by id (e.g. from the sidebar context menu).
    pub fn close_vtab(&mut self, id: VtabId) {
        self.leave(Leave::Vtab(id));
    }

    /// Close surface `sid` wherever it is, as the Close command would if it
    /// were the focused pane's active surface.
    pub fn close_surface_id(&mut self, sid: SurfaceId) {
        let found = self.tree.vtabs().iter().find_map(|v| {
            v.panes().iter().find(|p| p.surfaces.iter().any(|s| s.id == sid)).map(|p| (v.id, p.id))
        });
        if let Some((vt, pane)) = found {
            self.leave(Leave::Surface(vt, pane, sid));
        }
    }

    /// Close one surface (htab) within a pane; the tree collapses the pane, and
    /// then the vtab, if that was its last surface.
    pub fn close_surface(&mut self, vtab: VtabId, pane: PaneId, surface: SurfaceId) {
        self.leave(Leave::Surface(vtab, pane, surface));
    }

    /// Leave `what`, after asking when a view in it has unsaved changes.
    fn leave(&mut self, what: Leave) {
        if self.guard_unsaved {
            let unsaved = self.unsaved_in(what);
            if !unsaved.is_empty() {
                self.ask_unsaved(what, unsaved);
                return;
            }
        }
        self.perform(what);
    }

    /// The surfaces `what` removes, in tree order.
    fn surfaces_in(&self, what: Leave) -> Vec<SurfaceId> {
        let vtabs: Vec<&_> = match what {
            Leave::Surface(_, _, sid) => return vec![sid],
            Leave::Quit => self.tree.vtabs().iter().collect(),
            Leave::Pane(vt, _) | Leave::Vtab(vt) => self.tree.vtab(vt).into_iter().collect(),
        };
        vtabs
            .into_iter()
            .flat_map(|v| v.panes())
            .filter(|p| !matches!(what, Leave::Pane(_, pane) if pane != p.id))
            .flat_map(|p| p.surfaces.iter().map(|s| s.id))
            .collect()
    }

    /// The surfaces `what` removes whose views have unsaved changes.
    fn unsaved_in(&self, what: Leave) -> Vec<SurfaceId> {
        let mut ids = self.surfaces_in(what);
        ids.retain(|id| self.views.get(id).is_some_and(|v| v.modified()));
        ids
    }

    /// Open the save / cancel / discard question for leaving `what`. A view
    /// with no file has nowhere to save silently, so with one among them there
    /// is no save button.
    fn ask_unsaved(&mut self, what: Leave, unsaved: Vec<SurfaceId>) {
        let (message, save, can_save) = self.unsaved_question(&unsaved);
        let (accept, discard) = match what {
            Leave::Quit => ("Save and Quit", "Quit Without Saving"),
            _ => ("Save and Close", "Close Without Saving"),
        };
        self.confirm = Some(Confirm {
            message,
            accept: can_save.then_some(accept),
            discard: Some(discard),
            then: Confirmed::Unsaved { what, save },
            late: None,
        });
    }

    /// The unsaved-changes message for `unsaved`, the surfaces that can be
    /// saved to their file, and whether all of them can.
    fn unsaved_question(&self, unsaved: &[SurfaceId]) -> (String, Vec<SurfaceId>, bool) {
        let (save, scratch): (Vec<_>, Vec<_>) = unsaved
            .iter()
            .copied()
            .partition(|id| self.views.get(id).and_then(|v| v.path()).is_some());
        let subject = match unsaved {
            [one] => {
                let name = self.views.get(one).and_then(|v| v.title()).unwrap_or_default();
                format!("\u{201c}{name}\u{201d} has")
            }
            many => format!("{} buffers have", many.len()),
        };
        let mut message = format!("{subject} unsaved changes.");
        if !scratch.is_empty() {
            message.push_str(" A buffer with no file can't be saved here and would be lost.");
        }
        (message, save, scratch.is_empty())
    }

    /// Leave `what`: remove it from the tree and drop the views of surfaces
    /// that no longer exist, or flag the quit.
    fn perform(&mut self, what: Leave) {
        // Exiting mid-copy leaves a half-copied tree under the real name, so
        // the GUI asks first, after the unsaved-changes question. Cancel
        // leaves the app running and the copy going; quitting again later
        // asks again. Delete's checks don't count: they only read.
        if what == Leave::Quit && self.guard_unsaved && !self.file_jobs_idle() {
            self.confirm = Some(Confirm {
                message: "A file copy or move is still running. Quitting now would leave it half done.".into(),
                accept: Some("Quit Anyway"),
                discard: None,
                then: Confirmed::BusyQuit,
                late: None,
            });
            return;
        }
        self.prune_orphan_surfaces_after(|s| match what {
            Leave::Quit => {
                s.quitting = true;
                false
            }
            Leave::Surface(vt, pane, sid) => s.tree.close_surface(vt, pane, sid),
            Leave::Pane(vt, pane) => s.tree.close_pane(vt, pane),
            Leave::Vtab(vt) => s.tree.close_vtab(vt),
        });
    }

    /// Run a tree mutation, then drop terminals whose surfaces no longer exist.
    fn prune_orphan_surfaces_after(&mut self, f: impl FnOnce(&mut Self) -> bool) {
        f(self);
        let live: std::collections::HashSet<SurfaceId> = self
            .tree
            .vtabs()
            .iter()
            .flat_map(|v| v.panes())
            .flat_map(|p| p.surfaces.iter().map(|s| s.id))
            .collect();
        self.views.retain(|id, _| live.contains(id));
    }

    pub fn rename_active_vtab(&mut self, name: impl Into<String>) {
        if let Some(vt) = self.active() {
            if let Some(v) = self.tree.vtab_mut(vt) {
                v.name = name.into();
                v.user_named = true;
            }
        }
    }

    /// Focus a vtab. Unlike an email that opens as read, an unread tab is cleared
    /// by the auto-read *dwell* ([`tick_inbox`](Self::tick_inbox)), not instantly —
    /// so a quick glance doesn't consume it. Leaving a tab within the unfocus grace
    /// after it auto-read reverts it to unread.
    pub fn focus_vtab(&mut self, id: VtabId) {
        let prev = self.active();
        if !self.tree.focus_vtab(id) {
            return;
        }
        if prev == Some(id) {
            return;
        }
        // Unfocus grace: if we're leaving a tab that auto-read very recently, it
        // didn't really get read — put it back to unread.
        if let (Some(from), Some((ar_vt, ar_at))) = (prev, self.last_auto_read) {
            let grace = Duration::from_secs(self.inbox.auto_unread_before as u64);
            if from == ar_vt && ar_at.elapsed() < grace {
                let sticky = self
                    .tree
                    .vtab(from)
                    .map(|v| matches!(v.status, TabStatus::NeedsInput | TabStatus::Busy))
                    .unwrap_or(false);
                if !sticky {
                    self.tree
                        .set_status(from, TabStatus::Unread { success: true });
                }
            }
        }
        self.last_auto_read = None;
        self.focus_started = Instant::now();
    }

    /// Advance the auto-read dwell: an unread active tab focused continuously for
    /// `auto_read_after` seconds becomes read. `auto_read_after == 0` means manual
    /// only. Call periodically (each frame and on a scheduled deadline). Returns
    /// whether it changed a status.
    pub fn tick_inbox(&mut self) -> bool {
        let after = self.inbox.auto_read_after;
        if after == 0 {
            return false;
        }
        let Some(vt) = self.active() else {
            return false;
        };
        let is_unread = self
            .tree
            .vtab(vt)
            .map(|v| matches!(v.status, TabStatus::Unread { .. }))
            .unwrap_or(false);
        if is_unread && self.focus_started.elapsed() >= Duration::from_secs(after as u64) {
            self.tree.set_status(vt, TabStatus::Read);
            self.last_auto_read = Some((vt, Instant::now()));
            return true;
        }
        false
    }

    /// The next time an inbox timer should fire (a pending auto-read dwell), so the
    /// event loop can wake for it even without other activity.
    pub fn next_inbox_deadline(&self) -> Option<Instant> {
        let after = self.inbox.auto_read_after;
        if after == 0 {
            return None;
        }
        let vt = self.active()?;
        let is_unread = self
            .tree
            .vtab(vt)
            .map(|v| matches!(v.status, TabStatus::Unread { .. }))
            .unwrap_or(false);
        if is_unread {
            Some(self.focus_started + Duration::from_secs(after as u64))
        } else {
            None
        }
    }

    /// Set the active vtab's inbox status (used by mark-read / needs-input etc).
    pub fn set_active_status(&mut self, status: TabStatus) {
        if let Some(vt) = self.active() {
            self.tree.set_status(vt, status);
        }
    }

    fn vtab_of_surface(&self, sid: SurfaceId) -> Option<VtabId> {
        self.tree
            .vtabs()
            .iter()
            .find(|v| {
                v.panes()
                    .iter()
                    .any(|p| p.surfaces.iter().any(|s| s.id == sid))
            })
            .map(|v| v.id)
    }

    /// The active surface of the focused pane of the active vtab.
    pub fn focused_surface(&self) -> Option<SurfaceId> {
        let vt = self.active()?;
        let v = self.tree.vtab(vt)?;
        let pane = v.panes().into_iter().find(|p| p.id == v.focused_pane)?;
        pane.active_surface().map(|s| s.id)
    }

    /// Write raw bytes to a surface (agent-driven input). `false` if the
    /// surface's view takes no raw input (or there is no such surface).
    pub fn write_input(&mut self, id: SurfaceId, bytes: &[u8]) -> bool {
        self.views.get_mut(&id).is_some_and(|v| v.write_input(bytes))
    }

    /// Deliver a key press to the focused view, as the GUI does but without a
    /// window (no clipboard; default config and cell size), applying the
    /// workspace-level requests it makes. Returns whether the view used it.
    pub fn send_key_to_focused(&mut self, press: &KeyPress) -> bool {
        let Some(sid) = self.focused_surface() else {
            return false;
        };
        let Some(view) = self.views.get_mut(&sid) else {
            return false;
        };
        let cfg = Config::default();
        let mut cx = EventCx::new(&cfg, UiMetrics::default(), press.mods, None);
        let used = view.key(&mut cx, press);
        for req in cx.finish().requests {
            match req {
                Request::Busy => self.mark_busy(sid),
                Request::Close => self.close_surface_id(sid),
                _ => {}
            }
        }
        used
    }

    /// Show surface `sid`'s workspace busy right away (work was just submitted),
    /// holding it for a short grace until the real busy signal catches up. A
    /// sticky needs-input status is left alone.
    pub fn mark_busy(&mut self, sid: SurfaceId) {
        let Some(vt) = self.vtab_of_surface(sid) else {
            return;
        };
        let sticky = self
            .tree
            .vtab(vt)
            .map(|v| matches!(v.status, TabStatus::NeedsInput))
            .unwrap_or(false);
        if !sticky {
            self.tree.set_status(vt, TabStatus::Busy);
            self.optimistic_busy
                .insert(vt, Instant::now() + OPTIMISTIC_BUSY_GRACE);
        }
    }

    /// Pump every view's background output and sync view titles into the tree.
    /// Returns true if any view changed (e.g. a terminal produced output).
    pub fn pump_all(&mut self) -> bool {
        self.pump_all_budgeted(usize::MAX).0
    }

    /// Pump every view, bounding each to roughly `budget` bytes of output so a
    /// flood in one surface cannot monopolise a frame. Returns
    /// `(changed, more_pending)`: `more_pending` means at least one surface still
    /// has queued output and should be pumped again promptly.
    pub fn pump_all_budgeted(&mut self, budget: usize) -> (bool, bool) {
        let active = self.tree.active_vtab();
        let mut changed = self.poll_file_jobs();
        let mut more = false;
        let mut titles: Vec<(SurfaceId, String)> = Vec::new();
        let mut changed_surfaces: Vec<SurfaceId> = Vec::new();
        let mut finished: Vec<SurfaceId> = Vec::new();
        let close_on_exit = self.close_on_exit;
        for (id, view) in self.views.iter_mut() {
            let pumped = view.pump(budget);
            if let Some(exit) = view.downcast_mut::<TerminalView>().and_then(|t| t.exit()) {
                if close_on_exit.closes(exit == Lifecycle::Exited(Some(0))) {
                    finished.push(*id);
                }
            }
            if pumped.changed {
                changed = true;
                changed_surfaces.push(*id);
            }
            more |= pumped.more;
            changed |= view.refresh();
            if let Some(t) = view.title() {
                titles.push((*id, t));
            }
        }
        for (id, title) in titles {
            self.tree.set_surface_title(id, title, false);
        }
        if let Some((picker, _)) = &mut self.picker {
            changed |= picker.refresh();
        }
        // A terminal whose shell exited closes the way Close would.
        for sid in finished {
            self.close_surface_id(sid);
            changed = true;
        }
        // Output in a background vtab marks it unread (needs_input is sticky).
        for sid in changed_surfaces {
            let Some(vt) = self.vtab_of_surface(sid) else {
                continue;
            };
            if Some(vt) == active {
                continue;
            }
            let sticky = self
                .tree
                .vtab(vt)
                .map(|v| matches!(v.status, TabStatus::NeedsInput))
                .unwrap_or(false);
            if !sticky {
                self.tree
                    .set_status(vt, TabStatus::Unread { success: true });
            }
        }

        // Authoritative busy/idle from each vtab's foreground process group. Busy
        // overrides a fresh unread; finishing in the background surfaces as unread,
        // finishing in the foreground clears to read. NeedsInput stays sticky.
        let now = Instant::now();
        let busy_flags: Vec<(VtabId, bool)> = self
            .tree
            .vtabs()
            .iter()
            .map(|v| (v.id, self.vtab_is_busy(v.id)))
            .collect();
        for (vt, busy) in busy_flags {
            let optimistic = self
                .optimistic_busy
                .get(&vt)
                .map(|&until| now < until)
                .unwrap_or(false);
            let status = self.tree.vtab(vt).map(|v| v.status);
            match status {
                Some(TabStatus::NeedsInput) => {}
                _ if busy => self.tree.set_status(vt, TabStatus::Busy),
                Some(TabStatus::Busy) if !optimistic => {
                    // The command finished (and its optimistic grace has lapsed).
                    let done = if Some(vt) == active {
                        TabStatus::Read
                    } else {
                        TabStatus::Unread { success: true }
                    };
                    self.tree.set_status(vt, done);
                    self.optimistic_busy.remove(&vt);
                }
                _ => {}
            }
        }

        (changed, more)
    }

    /// Whether any of a vtab's surfaces has a foreground command running.
    fn vtab_is_busy(&self, vt: VtabId) -> bool {
        self.tree
            .vtab(vt)
            .map(|v| {
                v.panes()
                    .iter()
                    .flat_map(|p| p.surfaces.iter())
                    .any(|s| {
                        self.views.get(&s.id).is_some_and(|v| v.is_busy())
                    })
            })
            .unwrap_or(false)
    }

    /// Whether `id` has live content (not an empty surface).
    pub fn has_surface(&self, id: SurfaceId) -> bool {
        !self.surface_is_empty(id)
    }

    /// A human/agent-readable dump of the workspace structure.
    pub fn describe(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        let active = self.active();
        for v in self.tree.vtabs() {
            let act = if Some(v.id) == active {
                " (active)"
            } else {
                ""
            };
            let _ = writeln!(out, "vtab {} {:?} [{:?}]{}", v.id.0, v.name, v.status, act);
            for p in v.panes() {
                let foc = if p.id == v.focused_pane {
                    " (focused)"
                } else {
                    ""
                };
                let _ = writeln!(out, "  pane {}{}", p.id.0, foc);
                for (i, s) in p.surfaces.iter().enumerate() {
                    let a = if i == p.active { " (active)" } else { "" };
                    let title = if s.title.is_empty() {
                        "<untitled>"
                    } else {
                        &s.title
                    };
                    let _ = writeln!(out, "    surface {} {:?}{}", s.id.0, title, a);
                }
            }
        }
        if out.is_empty() {
            out.push_str("(no vtabs)\n");
        }
        out
    }

    /// A text rendering of one surface's content (a terminal is pumped first).
    pub fn surface_text(&mut self, id: SurfaceId) -> Option<String> {
        self.views.get_mut(&id)?.text()
    }

    /// A terminal surface's child state.
    pub fn surface_lifecycle(&mut self, id: SurfaceId) -> Option<Lifecycle> {
        let view = self.views.get_mut(&id)?;
        view.downcast_mut::<TerminalView>().map(|t| t.lifecycle())
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

fn failed(e: anyhow::Error) -> CmdError {
    CmdError::Failed(format!("{e:#}"))
}

/// Build the command registry over [`AppState`]. This is the one catalog the
/// palette, keybindings, and agent channel all use.
pub fn build_registry() -> Registry<AppState> {
    let mut r = Registry::new();

    r.register(
        CommandMeta::new(
            "workspace.new",
            "New Workspace",
            "Open a new empty workspace in a directory (chosen in a picker unless given)",
        )
        .arg(ArgSpec::optional("dir", ArgKind::Str, "the workspace directory")),
        Box::new(|s: &mut AppState, a| {
            if a.get("dir").is_none() {
                s.open_picker(Pick::NewWorkspace);
                return Ok(CmdOutcome::ok());
            }
            let id = s.new_workspace_in(a.get_str("dir")?);
            Ok(CmdOutcome::msg(format!("opened workspace {}", id.0)))
        }),
    );
    r.register(
        CommandMeta::new("workspace.close", "Close Workspace", "Close the active workspace"),
        Box::new(|s: &mut AppState, _| {
            s.close_active_vtab();
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new("workspace.rename", "Rename Workspace", "Rename the active workspace")
            .arg(ArgSpec::required("name", ArgKind::Str, "the new workspace name")),
        Box::new(|s: &mut AppState, a| {
            s.rename_active_vtab(a.get_str("name")?);
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "workspace.pick_dir",
            "Set Workspace Directory",
            "Choose the active workspace's directory in a floating file picker",
        ),
        Box::new(|s: &mut AppState, _| {
            s.open_picker(Pick::ActiveRoot);
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "split.leftright",
            "Split Left/Right",
            "Split the focused pane side by side",
        ),
        Box::new(|s: &mut AppState, _| {
            s.split_focused(Axis::LeftRight).map_err(failed)?;
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "split.topbottom",
            "Split Top/Bottom",
            "Split the focused pane stacked",
        ),
        Box::new(|s: &mut AppState, _| {
            s.split_focused(Axis::TopBottom).map_err(failed)?;
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "pane.close",
            "Close",
            "Close the focused tab; collapses the pane, then the workspace, when it was the last",
        ),
        Box::new(|s: &mut AppState, _| {
            s.close_focused_surface();
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "pane.close_all",
            "Close Pane",
            "Close the focused pane and every tab in it",
        ),
        Box::new(|s: &mut AppState, _| {
            s.close_focused_pane();
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "pane.focus_next",
            "Focus Next Pane",
            "Move focus to the next pane",
        ),
        Box::new(|s: &mut AppState, _| {
            s.focus_next_pane();
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "pane.focus_prev",
            "Focus Previous Pane",
            "Move focus to the previous pane",
        ),
        Box::new(|s: &mut AppState, _| {
            s.focus_prev_pane();
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "surface.new",
            "New Tab",
            "Add a new tab to the focused pane; pick what to open",
        ),
        Box::new(|s: &mut AppState, _| {
            s.add_empty_surface_in_focused();
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "file.open",
            "Open File",
            "Open a file in the focused pane with the plugin that handles it",
        )
        .arg(ArgSpec::required("path", ArgKind::Str, "the file path to open")),
        Box::new(|s: &mut AppState, a| {
            let path = std::path::PathBuf::from(a.get_str("path")?);
            s.open_path_in_focused(&path).map_err(failed)?;
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "workspace.mark_read",
            "Mark Workspace Read",
            "Clear the active workspace's inbox status",
        ),
        Box::new(|s: &mut AppState, _| {
            s.set_active_status(TabStatus::Read);
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "workspace.mark_unread",
            "Mark Workspace Unread",
            "Flag the active workspace as unread",
        ),
        Box::new(|s: &mut AppState, _| {
            s.set_active_status(TabStatus::Unread { success: true });
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "workspace.needs_input",
            "Mark Workspace Needs Input",
            "Flag the active workspace as blocked awaiting the user (agent self-report)",
        )
        .hidden(),
        Box::new(|s: &mut AppState, _| {
            s.set_active_status(TabStatus::NeedsInput);
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "workspace.dismiss",
            "Dismiss Workspace Status",
            "Clear a sticky needs-input flag",
        ),
        Box::new(|s: &mut AppState, _| {
            s.set_active_status(TabStatus::Read);
            Ok(CmdOutcome::ok())
        }),
    );

    files::register(&mut r);
    for plugin in crate::plugins::builtin() {
        plugin.register_commands(&mut r);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::editor::EditorView;
    use std::time::{Duration, Instant};

    #[test]
    fn the_settings_file_opens_with_its_language_service() {
        let dir = crate::plugin::testing::tmpdir("settings-lsp");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[nope]\n").unwrap();
        let catalogue = Arc::new(crate::settings::catalogue(&build_registry()));
        let languages = crate::languages::Languages::new(Some(path.clone()), catalogue);
        let mut s = AppState::new().with_languages(Arc::new(languages));
        s.open_settings(path).unwrap();
        let sid = s.focused_surface().expect("the settings editor is focused");
        let editor = s.view(sid).and_then(|v| v.downcast_ref::<EditorView>()).unwrap();
        assert!(editor.lang().is_some());

        // Another file gets none.
        let other = dir.join("notes.toml");
        std::fs::write(&other, "").unwrap();
        s.open_path_with_in_focused(editor::ID, &other).unwrap();
        let sid = s.focused_surface().unwrap();
        let editor = s.view(sid).and_then(|v| v.downcast_ref::<EditorView>()).unwrap();
        assert!(editor.lang().is_none());
    }

    #[test]
    fn new_vtab_spawns_a_surface() {
        let mut s = AppState::new().with_shell_line("sleep 1");
        let vt = s.new_vtab().unwrap();
        let v = s.tree.vtab(vt).unwrap();
        assert_eq!(v.panes().len(), 1);
        let surf = v.panes()[0].surfaces[0].id;
        assert!(
            s.view(surf).is_some_and(|v| v.plugin() == terminal::ID),
            "new_vtab should spawn a terminal for its surface"
        );
    }

    #[test]
    fn new_empty_vtab_opens_with_no_surface() {
        let mut s = AppState::new().with_shell_line("sleep 1");
        let vt = s.new_empty_vtab();
        let v = s.tree.vtab(vt).unwrap();
        assert_eq!(v.panes().len(), 1);
        assert!(
            v.panes()[0].surfaces.is_empty(),
            "a new workspace opens empty (nothing open)"
        );
        assert_eq!(s.tree.active_vtab(), Some(vt));
    }

    #[test]
    fn new_vtab_running_types_its_command_into_a_live_shell() {
        // Run Anything spawns an interactive shell and feeds it the command (so the
        // session stays live). `sh` reads and runs commands from the PTY.
        let mut s = AppState::new().with_shell_line("sh");
        let vt = s.new_vtab_running("printf RUN-MARKER; sleep 2").unwrap();
        let surf = s.tree.vtab(vt).unwrap().panes()[0].surfaces[0].id;

        let deadline = Instant::now() + Duration::from_secs(4);
        let mut text = String::new();
        while Instant::now() < deadline {
            text = s.surface_text(surf).unwrap_or_default();
            if text.contains("RUN-MARKER") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            text.contains("RUN-MARKER"),
            "new_vtab_running should run its own command, not the shell line; got:\n{text}"
        );
        // The vtab is named after the command's first token.
        assert_eq!(s.tree.vtab(vt).unwrap().name, "printf");
    }

    #[test]
    fn split_opens_an_empty_pane() {
        let mut s = AppState::new().with_shell_line("sleep 1");
        let vt = s.new_vtab().unwrap();
        s.split_focused(Axis::LeftRight).unwrap();
        let v = s.tree.vtab(vt).unwrap();
        assert_eq!(v.panes().len(), 2);
        // The new (focused) pane is empty — it opens on the picker, no terminal.
        let focused = v.panes().into_iter().find(|p| p.id == v.focused_pane).unwrap();
        assert!(focused.surfaces.is_empty(), "a new split opens empty");
    }

    #[test]
    fn picker_options_open_in_the_clicked_pane() {
        let mut s = AppState::new();
        let vt = s.new_empty_vtab();
        let left = s.tree.vtab(vt).unwrap().focused_pane;
        s.split_focused(Axis::LeftRight).unwrap();
        let right = s.tree.vtab(vt).unwrap().focused_pane;
        assert_ne!(left, right);

        s.open_plugin_in_pane(vt, left, editor::ID).unwrap();
        let v = s.tree.vtab(vt).unwrap();
        assert_eq!(v.focused_pane, left, "the clicked pane takes focus");
        let pane_surfaces = |id| v.panes().into_iter().find(|p| p.id == id).unwrap().surfaces.len();
        assert_eq!(pane_surfaces(left), 1);
        assert_eq!(pane_surfaces(right), 0, "the previously focused pane stays empty");
    }

    #[test]
    fn a_picker_option_for_a_workspace_that_is_no_longer_active_does_nothing() {
        let mut s = AppState::new();
        let first = s.new_empty_vtab();
        let pane = s.tree.vtab(first).unwrap().focused_pane;
        let second = s.new_empty_vtab();
        assert_eq!(s.active(), Some(second));

        s.open_plugin_in_pane(first, pane, editor::ID).unwrap();
        assert!(s.views.is_empty(), "nothing opened anywhere");
        assert_eq!(s.focused_surface(), None);
    }

    #[test]
    fn picker_options_are_ordered_with_the_web_browser_last() {
        let s = AppState::new();
        let titles: Vec<_> = s.plugins().iter().filter(|p| p.pickable()).map(|p| p.title()).collect();
        assert_eq!(titles, ["Terminal", "File browser", "New File", "Web browser"]);
    }

    #[test]
    fn closing_focused_pane_drops_its_terminal() {
        let mut s = AppState::new().with_shell_line("sleep 1");
        s.new_vtab().unwrap();
        // Split opens empty; materialise a terminal in it so there is one to drop.
        s.split_focused(Axis::TopBottom).unwrap();
        s.open_plugin_in_focused(terminal::ID).unwrap();
        let before = s.tree.surface_count();
        s.close_focused_pane();
        let after = s.tree.surface_count();
        assert_eq!(after, before - 1, "closing a pane should drop one surface");
        assert_eq!(
            s.views.len(),
            after,
            "orphaned terminals should be pruned"
        );
    }

    #[test]
    fn close_focused_surface_cascades_htab_then_empty_then_workspace() {
        let mut s = AppState::new().with_shell_line("sleep 1");
        let vt = s.new_vtab().unwrap();
        // Two htabs in the focused pane.
        s.new_surface_in_focused().unwrap();
        assert_eq!(s.tree.vtab(vt).unwrap().panes()[0].surfaces.len(), 2);

        // First close removes the active htab, leaving the pane and workspace.
        s.close_focused_surface();
        assert_eq!(s.tree.vtab(vt).unwrap().panes()[0].surfaces.len(), 1);
        assert_eq!(s.tree.vtab(vt).unwrap().panes().len(), 1);

        // Second close removes the last htab; the sole pane stays as the empty
        // "nothing open" state rather than closing the workspace.
        s.close_focused_surface();
        let v = s.tree.vtab(vt).expect("workspace survives as empty");
        assert!(v.panes()[0].surfaces.is_empty());
        assert_eq!(s.views.len(), s.tree.surface_count());

        // Closing again on the empty screen closes the workspace itself.
        s.close_focused_surface();
        assert!(s.tree.vtab(vt).is_none(), "empty workspace closes on next close");
    }

    #[test]
    fn close_focused_surface_collapses_only_the_split_when_other_panes_remain() {
        let mut s = AppState::new().with_shell_line("sleep 1");
        let vt = s.new_vtab().unwrap();
        s.split_focused(Axis::TopBottom).unwrap();
        assert_eq!(s.tree.vtab(vt).unwrap().panes().len(), 2);

        // The focused pane is the new empty split; closing it collapses the split
        // back to a single pane without closing the workspace.
        s.close_focused_surface();
        assert_eq!(s.tree.vtab(vt).unwrap().panes().len(), 1);
        assert!(s.tree.vtab(vt).is_some(), "workspace should survive");
        assert_eq!(s.views.len(), s.tree.surface_count());
    }

    #[test]
    fn background_output_marks_unread_and_dwell_reads() {
        // Both tabs print after a short delay; tab a is active, tab b is not.
        let mut s = AppState::new().with_shell_line("sleep 0.15; printf DONE; sleep 3");
        let a = s.new_vtab().unwrap();
        let b = s.new_vtab().unwrap();
        s.focus_vtab(a); // a active, b in the background

        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            s.pump_all();
            let b_status = s.tree.vtab(b).unwrap().status;
            if matches!(b_status, TabStatus::Unread { .. }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "background tab never went unread"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // a stayed read (its output arrived while it was active).
        assert_eq!(s.tree.vtab(a).unwrap().status, TabStatus::Read);

        // Focusing b no longer clears instantly — the auto-read dwell does.
        s.set_inbox_config(Inbox {
            auto_read_after: 1,
            auto_unread_before: 1,
        });
        s.focus_vtab(b);
        assert!(
            matches!(s.tree.vtab(b).unwrap().status, TabStatus::Unread { .. }),
            "focus alone must not clear unread; the dwell does"
        );
        s.tick_inbox();
        assert!(
            matches!(s.tree.vtab(b).unwrap().status, TabStatus::Unread { .. }),
            "before the dwell elapses it stays unread"
        );
        std::thread::sleep(Duration::from_millis(1100));
        s.tick_inbox();
        assert_eq!(
            s.tree.vtab(b).unwrap().status,
            TabStatus::Read,
            "after the dwell, sustained focus marks it read"
        );
    }

    #[test]
    fn leaving_within_grace_reverts_auto_read() {
        let mut s = AppState::new().with_shell_line("sleep 5");
        let a = s.new_vtab().unwrap();
        let b = s.new_vtab().unwrap();
        s.set_inbox_config(Inbox {
            auto_read_after: 1,
            auto_unread_before: 5,
        });
        s.focus_vtab(a); // a active, b background
        s.tree.set_status(b, TabStatus::Unread { success: true });

        // Focus b, dwell until it auto-reads.
        s.focus_vtab(b);
        std::thread::sleep(Duration::from_millis(1100));
        s.tick_inbox();
        assert_eq!(s.tree.vtab(b).unwrap().status, TabStatus::Read);

        // Leaving b immediately (within the grace) reverts the auto-read.
        s.focus_vtab(a);
        assert!(
            matches!(s.tree.vtab(b).unwrap().status, TabStatus::Unread { .. }),
            "leaving within the unfocus grace should revert the auto-read to unread"
        );
    }

    #[test]
    fn new_surface_starts_in_the_workspace_root_dir() {
        let dir = std::env::temp_dir().join(format!("ghostrealm-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.file_name().unwrap().to_string_lossy().into_owned();

        // `sh` reads commands from the PTY; a surface spawned with a pinned root dir
        // starts there, so `pwd` prints a path containing the marker directory.
        let mut s = AppState::new().with_shell_line("sh");
        s.new_vtab().unwrap();
        s.set_active_root_dir(dir.clone());
        s.new_surface_in_focused().unwrap();
        let surf = s.focused_surface().unwrap();
        assert!(s.write_input(surf, b"pwd\n"));

        let deadline = Instant::now() + Duration::from_secs(4);
        let mut text = String::new();
        while Instant::now() < deadline {
            text = s.surface_text(surf).unwrap_or_default();
            if text.contains(&marker) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir(&dir);
        assert!(
            text.contains(&marker),
            "a surface in a workspace with a pinned root should start there; pwd:\n{text}"
        );
    }

    #[test]
    fn open_editor_creates_a_focused_editor_surface() {
        let mut s = AppState::new().with_shell_line("sleep 2");
        s.new_vtab().unwrap();
        s.open_plugin_in_focused(editor::ID).unwrap();

        let surf = s.focused_surface().unwrap();
        let view = s.view_mut(surf).expect("the opened editor becomes focused");
        assert_eq!(view.plugin(), editor::ID);
        let ed = view.downcast_mut::<EditorView>().expect("an EditorView");
        ed.buffer_mut().insert_char('x');
        assert_eq!(s.surface_text(surf).as_deref(), Some("x"));
        assert!(
            s.surface_lifecycle(surf).is_none(),
            "an editor surface has no terminal"
        );
    }

    #[test]
    fn opening_a_path_picks_the_claiming_plugin() {
        let mut s = AppState::new();
        s.new_empty_vtab();
        let dir = crate::plugin::testing::tmpdir("app-open");
        let file = dir.join("notes.txt");
        std::fs::write(&file, "hello").unwrap();
        let mut r = build_registry();
        let mut args = ghostrealm_core::Args::new();
        args.insert("path", ghostrealm_core::Value::Str(file.display().to_string()));
        r.execute("file.open", &args, &mut s).expect("file.open runs");
        let sid = s.focused_surface().unwrap();
        assert_eq!(s.view(sid).map(|v| v.plugin()), Some(editor::ID));
        assert_eq!(s.surface_text(sid).as_deref(), Some("hello"));
        let title = s.tree.vtabs()[0].panes()[0].surfaces.last().unwrap().title.clone();
        assert_eq!(title, "notes.txt", "the view titles its tab");
    }

    #[test]
    fn close_vtab_by_id_removes_it_and_prunes_its_surface() {
        let mut s = AppState::new().with_shell_line("sleep 2");
        let a = s.new_vtab().unwrap();
        let b = s.new_vtab().unwrap();
        let before = s.tree.vtabs().len();
        let surfaces_before = s.views.len();

        s.close_vtab(a);

        assert_eq!(s.tree.vtabs().len(), before - 1);
        assert!(s.tree.vtab(a).is_none(), "the closed vtab is gone");
        assert!(s.tree.vtab(b).is_some(), "other vtabs remain");
        assert_eq!(
            s.views.len(),
            surfaces_before - 1,
            "the closed vtab's terminal is pruned"
        );
    }

    #[test]
    fn reading_a_background_surface_still_marks_it_unread() {
        let mut s = AppState::new().with_shell_line("sleep 0.15; printf DONE; sleep 3");
        let a = s.new_vtab().unwrap();
        let b = s.new_vtab().unwrap();
        let sid = s.focused_surface().unwrap();
        s.focus_vtab(a);
        s.pump_all();
        let deadline = Instant::now() + Duration::from_secs(4);
        while !s.surface_text(sid).unwrap_or_default().contains("DONE") {
            assert!(Instant::now() < deadline, "no output");
            std::thread::sleep(Duration::from_millis(20));
        }
        s.pump_all();
        assert!(
            matches!(s.tree.vtab(b).unwrap().status, TabStatus::Unread { .. }),
            "an agent read must not hide the output from the inbox"
        );
    }

    #[test]
    fn mark_unread_command_sets_unread() {
        let mut s = AppState::new().with_shell_line("sleep 2");
        let vt = s.new_vtab().unwrap();
        assert_eq!(s.tree.vtab(vt).unwrap().status, TabStatus::Read);
        let mut r = build_registry();
        r.execute("workspace.mark_unread", &ghostrealm_core::Args::new(), &mut s)
            .expect("workspace.mark_unread runs");
        assert!(
            matches!(s.tree.vtab(vt).unwrap().status, TabStatus::Unread { .. }),
            "workspace.mark_unread should set the active tab unread"
        );
    }

    #[test]
    fn file_browser_opens_as_a_plugin_view_in_the_empty_slot() {
        let mut s = AppState::new();
        let dir = crate::plugin::testing::tmpdir("app-fb");
        std::fs::write(dir.join("hello.txt"), "x").unwrap();
        s.set_default_dir(Some(dir.clone()));
        s.new_empty_vtab();
        let mut r = build_registry();
        r.execute("file_browser.new", &ghostrealm_core::Args::new(), &mut s)
            .expect("file_browser.new runs");
        let sid = s.focused_surface().expect("the empty pane got a surface");
        assert_eq!(s.view(sid).map(|v| v.plugin()), Some("file_browser"));
        assert!(!s.surface_is_empty(sid));
        let text = s.surface_text(sid).expect("a view reports its text");
        assert!(text.contains("hello.txt"), "rooted at the workspace dir: {text}");
        // A second open adds a tab rather than replacing the browser.
        r.execute("file_browser.new", &ghostrealm_core::Args::new(), &mut s)
            .unwrap();
        assert_ne!(s.focused_surface(), Some(sid));
    }

    #[test]
    fn picker_confirm_pins_the_workspace_root() {
        let mut s = AppState::new();
        let dir = crate::plugin::testing::tmpdir("app-picker");
        s.set_default_dir(Some(dir.clone()));
        let vt = s.new_empty_vtab();
        s.open_picker(Pick::ActiveRoot);
        assert!(s.picker_open());
        let sub = dir.join("sub");
        std::fs::create_dir(&sub).unwrap();
        s.picker_mut().unwrap().tree_mut().set_root(&sub);
        s.confirm_picker().unwrap();
        assert!(!s.picker_open());
        assert_eq!(s.tree.vtab(vt).unwrap().root_dir.as_deref(), Some(sub.as_path()));
    }

    #[test]
    fn editor_open_without_a_path_picks_the_file() {
        use ghostrealm_core::Args;
        let mut r = build_registry();
        let mut s = AppState::new();
        let dir = crate::plugin::testing::tmpdir("app-open-pick");
        let file = dir.join("notes.txt");
        std::fs::write(&file, "hi").unwrap();
        s.set_default_dir(Some(dir));
        s.new_empty_vtab();

        r.execute("editor.open", &Args::new(), &mut s).unwrap();
        assert_eq!(s.picker_purpose(), Some(Pick::OpenWith(editor::ID)));
        s.confirm_picker().unwrap();
        assert!(s.picker_open(), "nothing chosen: the picker stays open");

        s.confirm_picker_with(file.clone()).unwrap();
        assert!(!s.picker_open());
        let sid = s.focused_surface().unwrap();
        let ed = s.view(sid).unwrap().downcast_ref::<EditorView>().unwrap();
        assert_eq!(ed.buffer().path.as_deref(), Some(file.as_path()));
    }

    #[test]
    fn a_file_that_is_not_text_does_not_open() {
        let mut s = AppState::new();
        let file = crate::plugin::testing::tmpdir("app-open-binary").join("latin1.txt");
        std::fs::write(&file, b"caf\xe9").unwrap();
        s.new_empty_vtab();
        let err = s.open_path_in_focused(&file).unwrap_err();
        assert!(format!("{err:#}").contains("latin1.txt"), "{err:#}");
        assert_eq!(s.focused_surface(), None, "no tab bound to it");
    }

    #[test]
    fn a_settings_file_that_wont_open_leaves_the_focus_alone() {
        let path = crate::plugin::testing::tmpdir("settings-unreadable").join("config.toml");
        std::fs::write(&path, "").unwrap();
        let mut s = AppState::new();
        s.open_settings(path.clone()).unwrap();
        s.close_focused_surface();
        let elsewhere = s.new_empty_vtab();
        std::fs::write(&path, b"caf\xe9").unwrap();
        assert!(s.open_settings(path).is_err());
        assert_eq!(s.active(), Some(elsewhere), "not moved to an empty settings workspace");
    }

    #[cfg(unix)]
    #[test]
    fn autosaving_a_symlinked_config_reports_the_config() {
        let dir = crate::plugin::testing::tmpdir("autosave-config-link");
        let real = dir.join("config.toml");
        std::fs::write(&real, "").unwrap();
        let link = dir.join("link.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut s = AppState::new();
        s.new_empty_vtab();
        s.open_path_with_in_focused(editor::ID, &link).unwrap();
        let sid = s.focused_surface().unwrap();
        s.views.get_mut(&sid).unwrap().write_input(b"x");
        let mut cfg = Config::default();
        cfg.editor.autosave_on_unfocus = true;
        assert!(s.autosave_all_with_config(&cfg, Some(&real)), "the link is the config");
    }

    #[test]
    fn save_as_picks_beside_the_file_and_retitles_the_tab() {
        use ghostrealm_core::Args;
        let mut r = build_registry();
        let mut s = AppState::new();
        let dir = crate::plugin::testing::tmpdir("app-save-as");
        let file = dir.join("old.txt");
        std::fs::write(&file, "x").unwrap();
        s.new_empty_vtab();
        s.open_path_with_in_focused(editor::ID, &file).unwrap();
        let sid = s.focused_surface().unwrap();

        r.execute("editor.save_as", &Args::new(), &mut s).unwrap();
        assert_eq!(s.picker_purpose(), Some(Pick::SaveAs(sid)));
        let target = dir.join("old.txt");
        assert_eq!(s.picker_mut().unwrap().choice(), Some(target), "beside it, same name");

        let new = dir.join("new.txt");
        assert_eq!(s.confirm_picker_with(new.clone()).unwrap(), Some(new.clone()));
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "x\n");
        let vt = s.active().unwrap();
        let title = s.tree.vtab(vt).unwrap().panes().into_iter().find_map(|p| {
            p.surfaces.iter().find(|x| x.id == sid).map(|x| x.title.clone())
        });
        assert_eq!(title.as_deref(), Some("new.txt"));
    }

    #[test]
    fn toggle_comment_comments_the_focused_editors_lines_per_its_language() {
        use ghostrealm_core::Args;
        let mut r = build_registry();
        let mut s = AppState::new();
        let dir = crate::plugin::testing::tmpdir("app-toggle-comment");
        let toml = dir.join("a.toml");
        std::fs::write(&toml, "a = 1\nb = 2\n").unwrap();
        s.new_empty_vtab();
        s.open_path_with_in_focused(editor::ID, &toml).unwrap();
        let sid = s.focused_surface().unwrap();
        let lines = |s: &AppState| s.view(sid).unwrap().downcast_ref::<EditorView>().unwrap().buffer().lines.clone();

        r.execute("editor.toggle_comment", &Args::new(), &mut s).unwrap();
        assert_eq!(lines(&s), ["# a = 1", "b = 2"]);
        r.execute("editor.toggle_comment", &Args::new(), &mut s).unwrap();
        assert_eq!(lines(&s), ["a = 1", "b = 2"]);

        // A file whose language has no comment syntax reports it and stays as it was.
        let txt = dir.join("a.txt");
        std::fs::write(&txt, "x").unwrap();
        s.open_path_with_in_focused(editor::ID, &txt).unwrap();
        assert!(r.execute("editor.toggle_comment", &Args::new(), &mut s).is_err());
    }

    #[test]
    fn save_as_over_another_file_asks_first() {
        let mut s = AppState::new();
        let dir = crate::plugin::testing::tmpdir("app-replace");
        let (mine, theirs) = (dir.join("mine.txt"), dir.join("theirs.txt"));
        std::fs::write(&mine, "mine").unwrap();
        std::fs::write(&theirs, "theirs").unwrap();
        s.new_empty_vtab();
        s.open_path_with_in_focused(editor::ID, &mine).unwrap();
        let sid = s.focused_surface().unwrap();

        s.open_save_as(sid);
        assert_eq!(s.confirm_picker_with(mine.clone()).unwrap(), Some(mine.clone()), "its own file: no question");

        s.open_save_as(sid);
        assert_eq!(s.confirm_picker_with(theirs.clone()).unwrap(), None);
        assert_eq!(s.confirm().and_then(|c| c.accept), Some("Replace"));
        assert!(s.picker_open(), "the picker waits behind the question");
        s.cancel_confirm();
        assert!(s.picker_open() && s.confirm().is_none());
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "theirs", "nothing written");

        s.confirm_picker_with(theirs.clone()).unwrap();
        assert_eq!(s.accept_confirm().unwrap(), vec![theirs.clone()]);
        assert!(!s.picker_open());
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "mine\n");
    }

    #[test]
    fn new_workspace_is_created_only_once_its_directory_is_picked() {
        use ghostrealm_core::{Args, Value};
        let mut r = build_registry();
        let mut s = AppState::new();
        let dir = crate::plugin::testing::tmpdir("app-new-ws");
        s.set_default_dir(Some(dir.clone()));
        let count = |s: &AppState| s.tree.vtabs().len();

        r.execute("workspace.new", &Args::new(), &mut s).unwrap();
        assert_eq!(s.picker_purpose(), Some(Pick::NewWorkspace));
        assert_eq!(count(&s), 0, "nothing is created before a directory is picked");
        s.close_picker();
        assert_eq!(count(&s), 0, "cancelling creates nothing");

        r.execute("workspace.new", &Args::new(), &mut s).unwrap();
        let proj = dir.join("proj");
        std::fs::create_dir(&proj).unwrap();
        s.picker_mut().unwrap().tree_mut().set_root(&proj);
        s.confirm_picker().unwrap();
        assert_eq!(count(&s), 1);
        let vt = s.active().unwrap();
        assert_eq!(s.tree.vtab(vt).unwrap().root_dir.as_deref(), Some(proj.as_path()));

        // The picker starts at the default directory, not the active workspace's.
        r.execute("workspace.new", &Args::new(), &mut s).unwrap();
        assert_eq!(s.picker().unwrap().tree().root(), dir.as_path());
        s.close_picker();

        // Given a directory, the command skips the picker.
        let args = Args::new().with("dir", Value::Str(dir.display().to_string()));
        r.execute("workspace.new", &args, &mut s).unwrap();
        assert!(!s.picker_open());
        assert_eq!(count(&s), 2);
        let vt = s.active().unwrap();
        assert_eq!(s.tree.vtab(vt).unwrap().root_dir.as_deref(), Some(dir.as_path()));
    }

    #[test]
    fn enter_marks_active_busy_optimistically_then_clears() {
        use ghostrealm_terminal::{Key, Mods};

        // `sh -c cat` execs cat (no forked foreground group), so is_busy() stays
        // false — this exercises the optimistic-busy grace, not the pgrp path.
        let mut s = AppState::new().with_shell_line("cat");
        let vt = s.new_vtab().unwrap();
        assert_eq!(s.tree.vtab(vt).unwrap().status, TabStatus::Read);

        s.send_key_to_focused(&KeyPress {
            key: Key::Enter,
            mods: Mods::default(),
            text: None,
        });
        assert_eq!(
            s.tree.vtab(vt).unwrap().status,
            TabStatus::Busy,
            "Enter should optimistically mark the active vtab busy"
        );

        // Within the grace window, an idle pump keeps it busy.
        s.pump_all();
        assert_eq!(s.tree.vtab(vt).unwrap().status, TabStatus::Busy);

        // Once the grace lapses, the authoritative idle state clears it to read.
        std::thread::sleep(OPTIMISTIC_BUSY_GRACE + Duration::from_millis(80));
        s.pump_all();
        assert_eq!(
            s.tree.vtab(vt).unwrap().status,
            TabStatus::Read,
            "after the grace lapses and no command is running, busy clears to read"
        );
    }

    /// Pump `s` until `done` (or 5 s pass); whether it held.
    fn pump_until(s: &mut AppState, done: impl Fn(&mut AppState) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            s.pump_all();
            if done(s) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Whether workspace `vt` has no surface left.
    fn emptied(s: &AppState, vt: VtabId) -> bool {
        s.tree.vtab(vt).is_some_and(|v| v.panes().iter().all(|p| p.surfaces.is_empty()))
    }

    fn enter() -> KeyPress {
        KeyPress { key: ghostrealm_terminal::Key::Enter, mods: ghostrealm_terminal::Mods::default(), text: None }
    }

    #[test]
    fn a_shell_that_exits_with_code_0_closes_its_surface() {
        let mut s = AppState::new().with_shell_line("exit 0");
        let vt = s.new_vtab().unwrap();
        assert!(pump_until(&mut s, |s| emptied(s, vt)), "the surface closes\n{}", s.describe());
    }

    #[test]
    fn a_failed_shell_stays_with_a_banner_and_any_key_closes_it() {
        let mut s = AppState::new().with_shell_line("exit 3");
        let vt = s.new_vtab().unwrap();
        let sid = s.focused_surface().unwrap();
        let banner = "exited with code 3 \u{2014} press any key to close";
        assert!(
            pump_until(&mut s, |s| s.surface_text(sid).is_some_and(|t| t.contains(banner))),
            "{:?}",
            s.surface_text(sid)
        );
        assert!(!emptied(&s, vt), "kept");
        s.pump_all();
        assert!(!emptied(&s, vt), "still kept after more pumps");
        assert!(s.send_key_to_focused(&enter()));
        assert!(emptied(&s, vt), "a key closes it");
    }

    #[test]
    fn the_exit_banner_does_not_hide_a_last_line_with_output_on_it() {
        // Fills the screen and ends on a line with no newline: the cursor's
        // row, the last, holds output.
        let mut s = AppState::new()
            .with_shell_line("i=0; while [ $i -lt 80 ]; do echo line$i; i=$((i+1)); done; printf 'fatal: last'; exit 3");
        s.new_vtab().unwrap();
        let sid = s.focused_surface().unwrap();
        // The exit can be seen before the last output is read: wait for both.
        let both = |t: &str| t.contains("exited with code 3") && t.contains("fatal: last");
        assert!(
            pump_until(&mut s, |s| s.surface_text(sid).is_some_and(|t| both(&t))),
            "the last line is kept:\n{}",
            s.surface_text(sid).unwrap_or_default()
        );
    }

    #[test]
    fn a_shell_killed_by_a_signal_stays_with_the_signal_named() {
        let mut s = AppState::new().with_shell_line("kill -9 $$");
        let vt = s.new_vtab().unwrap();
        let sid = s.focused_surface().unwrap();
        let banner = "killed by signal SIGKILL \u{2014} press any key to close";
        assert!(
            pump_until(&mut s, |s| s.surface_text(sid).is_some_and(|t| t.contains(banner))),
            "{:?}",
            s.surface_text(sid)
        );
        assert!(!emptied(&s, vt));
    }

    #[test]
    fn close_on_exit_chooses_which_exits_close() {
        // always: a failure closes too.
        let mut s = AppState::new().with_shell_line("exit 1");
        s.set_close_on_exit(CloseOnExit::Always);
        let vt = s.new_vtab().unwrap();
        assert!(pump_until(&mut s, |s| emptied(s, vt)));

        // never: even a clean exit stays, with a banner.
        let mut s = AppState::new().with_shell_line("exit 0");
        s.set_close_on_exit(CloseOnExit::Never);
        let vt = s.new_vtab().unwrap();
        let sid = s.focused_surface().unwrap();
        let banner = "exited with code 0 \u{2014} press any key to close";
        assert!(pump_until(&mut s, |s| s.surface_text(sid).is_some_and(|t| t.contains(banner))));
        assert!(!emptied(&s, vt));
        // The text shows the banner from the terminal's lifecycle; the view
        // takes the exit (and with it, any key closing) at its next pump.
        s.pump_all();
        assert!(s.send_key_to_focused(&enter()));
        assert!(emptied(&s, vt));
    }

    #[test]
    fn a_background_tab_whose_shell_exits_closes_only_itself() {
        let mut s = AppState::new().with_shell_line("sleep 30");
        let vt = s.new_vtab().unwrap();
        let first = s.focused_surface().unwrap();
        s.shell_line = Some("sleep 0.2".into());
        s.open_plugin_in_focused(terminal::ID).unwrap();
        let second = s.focused_surface().unwrap();
        let pane = s.tree.vtab(vt).unwrap().focused_pane;
        // Focus goes back to the first, so the second is the background tab.
        s.tree.vtab_mut(vt).unwrap().pane_mut(pane).unwrap().active = 0;
        assert!(pump_until(&mut s, |s| s.surface_text(second).is_none()), "the exited tab closes");
        assert_eq!(s.focused_surface(), Some(first));
        assert!(s.surface_text(first).is_some(), "the other tab stays");
    }

    #[test]
    fn needs_input_is_sticky_over_output() {
        let mut s = AppState::new().with_shell_line("sleep 0.15; printf X; sleep 3");
        let a = s.new_vtab().unwrap();
        let b = s.new_vtab().unwrap();
        s.focus_vtab(a);
        s.tree.set_status(b, TabStatus::NeedsInput);

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            s.pump_all();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            s.tree.vtab(b).unwrap().status,
            TabStatus::NeedsInput,
            "needs_input must not be overwritten by background output"
        );
    }

    #[test]
    fn write_input_reaches_child() {
        // `cat` echoes its stdin back through the PTY.
        let mut s = AppState::new().with_shell_line("cat");
        let vt = s.new_vtab().unwrap();
        let surf = s.tree.vtab(vt).unwrap().panes()[0].surfaces[0].id;
        assert!(s.write_input(surf, b"hello-input\r"));

        let deadline = Instant::now() + Duration::from_secs(4);
        let mut text = String::new();
        while Instant::now() < deadline {
            text = s.surface_text(surf).unwrap_or_default();
            if text.contains("hello-input") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            text.contains("hello-input"),
            "input written to a surface should appear in its grid, got:\n{text}"
        );
    }

    #[test]
    fn surface_captures_child_output() {
        let mut s = AppState::new().with_shell_line("printf 'READY-MARKER'; sleep 2");
        let vt = s.new_vtab().unwrap();
        let surf = s.tree.vtab(vt).unwrap().panes()[0].surfaces[0].id;

        let deadline = Instant::now() + Duration::from_secs(4);
        let mut text = String::new();
        while Instant::now() < deadline {
            text = s.surface_text(surf).unwrap_or_default();
            if text.contains("READY-MARKER") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            text.contains("READY-MARKER"),
            "surface grid should capture child output within the timeout, got:\n{text}\n\
             Next steps: check spawn_surface wiring and pump/snapshot in surface_text."
        );
    }

    /// A guarded state with one workspace whose focused pane has `name` open
    /// in `dir` (containing "old\n") and edited.
    fn edited(dir: &Path, name: &str) -> (AppState, SurfaceId, std::path::PathBuf) {
        let mut s = AppState::new().with_unsaved_guard();
        s.new_empty_vtab();
        let file = dir.join(name);
        std::fs::write(&file, "old\n").unwrap();
        s.open_path_with_in_focused(editor::ID, &file).unwrap();
        let sid = s.focused_surface().unwrap();
        assert!(s.views.get_mut(&sid).unwrap().write_input(b"new"));
        assert!(s.view(sid).unwrap().modified());
        (s, sid, file)
    }

    fn surface_count(s: &AppState) -> usize {
        s.tree.vtabs().iter().flat_map(|v| v.panes()).map(|p| p.surfaces.len()).sum()
    }

    #[test]
    fn closing_an_unmodified_view_asks_nothing() {
        let dir = crate::plugin::testing::tmpdir("close-clean");
        let mut s = AppState::new().with_unsaved_guard();
        s.new_empty_vtab();
        s.open_path_with_in_focused(editor::ID, &dir.join("new.txt")).unwrap();
        assert!(!s.view(s.focused_surface().unwrap()).unwrap().modified());
        assert!(!s.has_unsaved());
        s.close_focused_surface();
        assert!(s.confirm().is_none());
        assert_eq!(surface_count(&s), 0);
        s.request_quit();
        assert!(s.confirm().is_none() && s.quitting());
    }

    #[test]
    fn closing_a_modified_tab_asks_and_each_button_decides() {
        let dir = crate::plugin::testing::tmpdir("close-tab");
        // Cancel: nothing closes, nothing is written.
        let (mut s, sid, file) = edited(&dir, "a.txt");
        s.close_focused_surface();
        let c = s.confirm().expect("a modified view asks first");
        assert_eq!(
            (c.accept, c.discard),
            (Some("Save and Close"), Some("Close Without Saving"))
        );
        assert!(c.message.contains("a.txt"), "{}", c.message);
        s.cancel_confirm();
        assert!(s.view(sid).is_some());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old\n");

        // Close without saving: the tab goes, the file is untouched.
        s.close_focused_surface();
        s.discard_confirm().unwrap();
        assert!(s.confirm().is_none() && s.view(sid).is_none());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old\n");

        // Save and close: the file is written, then the tab goes.
        let (mut s, sid, file) = edited(&dir, "b.txt");
        s.close_focused_surface();
        assert_eq!(s.accept_confirm().unwrap(), vec![file.clone()]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "newold\n");
        assert!(s.view(sid).is_none());
    }

    /// `edited`, then another program rewrites the file.
    fn conflicted(dir: &Path, name: &str) -> (AppState, SurfaceId, std::path::PathBuf) {
        let (s, sid, file) = edited(dir, name);
        std::fs::write(&file, "theirs\n").unwrap();
        (s, sid, file)
    }

    fn lines(s: &AppState, sid: SurfaceId) -> Vec<String> {
        s.view(sid).and_then(|v| v.downcast_ref::<EditorView>()).unwrap().buffer().lines.clone()
    }

    #[test]
    fn enter_saves_at_the_unsaved_question_but_never_overwrites_a_changed_file() {
        let dir = crate::plugin::testing::tmpdir("conflict-enter");
        // Unsaved changes: Enter saves.
        let (mut s, sid, file) = edited(&dir, "a.txt");
        s.close_focused_surface();
        assert!(!s.confirm().unwrap().enter_cancels());
        assert_eq!(s.enter_confirm().unwrap(), vec![file.clone()]);
        assert!(s.view(sid).is_none());

        // A changed file: the Enter that saves reaches the changed-file
        // question, and a second Enter cancels it.
        let (mut s, sid, file) = conflicted(&dir, "b.txt");
        s.close_focused_surface();
        assert!(s.enter_confirm().unwrap().is_empty());
        assert!(s.confirm().unwrap().enter_cancels());
        assert!(s.enter_confirm().unwrap().is_empty());
        assert!(s.confirm().is_none());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs\n");
        assert!(s.view(sid).unwrap().modified());

        // Likewise for an explicit save.
        s.ask_disk_conflict(sid);
        assert!(s.enter_confirm().unwrap().is_empty());
        assert!(s.confirm().is_none());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs\n");
        assert_eq!(lines(&s, sid), ["newold"]);
    }

    #[test]
    fn a_save_conflict_asks_and_overwrite_writes() {
        let dir = crate::plugin::testing::tmpdir("conflict-overwrite");
        let (mut s, sid, file) = conflicted(&dir, "a.txt");
        s.ask_disk_conflict(sid);
        let c = s.confirm().expect("asks");
        assert_eq!((c.accept, c.discard), (Some("Overwrite"), Some("Reload")));
        assert_eq!(c.message, "\u{201c}a.txt\u{201d} changed on disk since you opened it.");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs\n", "nothing written yet");
        assert_eq!(s.accept_confirm().unwrap(), vec![file.clone()]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "newold\n");
        assert!(!s.view(sid).unwrap().modified() && !s.view(sid).unwrap().disk_conflict());
        assert!(s.confirm().is_none());
    }

    #[test]
    fn a_save_conflict_reload_takes_the_disk_version_and_cancel_changes_nothing() {
        let dir = crate::plugin::testing::tmpdir("conflict-reload");
        let (mut s, sid, file) = conflicted(&dir, "a.txt");
        s.ask_disk_conflict(sid);
        s.cancel_confirm();
        assert_eq!(lines(&s, sid), ["newold"]);
        assert!(s.view(sid).unwrap().modified());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs\n");

        s.ask_disk_conflict(sid);
        assert!(s.discard_confirm().unwrap().is_empty());
        assert_eq!(lines(&s, sid), ["theirs"]);
        assert!(!s.view(sid).unwrap().modified() && !s.view(sid).unwrap().disk_conflict());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs\n");
        assert!(s.confirm().is_none());
    }

    #[test]
    fn save_and_close_asks_about_a_changed_file_before_closing() {
        let dir = crate::plugin::testing::tmpdir("conflict-close");
        // Overwrite: written, then closed.
        let (mut s, sid, file) = conflicted(&dir, "a.txt");
        s.close_focused_surface();
        assert!(s.accept_confirm().unwrap().is_empty(), "stopped to ask: nothing written");
        let c = s.confirm().expect("the changed-file question");
        assert_eq!(c.accept, Some("Overwrite"));
        assert!(s.view(sid).is_some());
        assert_eq!(s.accept_confirm().unwrap(), vec![file.clone()]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "newold\n");
        assert!(s.view(sid).is_none(), "closed after the write");

        // Reload: the disk version is shown and the close stops.
        let (mut s, sid, file) = conflicted(&dir, "b.txt");
        s.close_focused_surface();
        s.accept_confirm().unwrap();
        s.discard_confirm().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs\n");
        assert_eq!(lines(&s, sid), ["theirs"]);
        assert!(!s.view(sid).unwrap().modified());
        assert!(s.confirm().is_none());

        // Cancel: stays open, unsaved.
        let (mut s, sid, file) = conflicted(&dir, "c.txt");
        s.close_focused_surface();
        s.accept_confirm().unwrap();
        s.cancel_confirm();
        assert!(s.view(sid).unwrap().modified());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs\n");
    }

    #[test]
    fn autosave_on_blur_or_quit_leaves_a_changed_file_alone() {
        let dir = crate::plugin::testing::tmpdir("conflict-autosave");
        let (mut s, sid, file) = conflicted(&dir, "a.txt");
        let cfg = Config::default();
        assert!(cfg.editor.autosave_on_unfocus);
        s.autosave_all(&cfg);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs\n");
        assert!(s.view(sid).unwrap().modified());
        assert!(s.confirm().is_none(), "no dialog for an autosave");
    }

    #[test]
    fn save_as_over_its_own_changed_file_asks_to_replace() {
        let dir = crate::plugin::testing::tmpdir("conflict-save-as");
        let (mut s, sid, file) = conflicted(&dir, "a.txt");
        s.open_save_as(sid);
        s.confirm_picker_with(file.clone()).unwrap();
        assert_eq!(s.confirm().and_then(|c| c.accept), Some("Replace"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs\n");
    }

    #[test]
    fn closing_a_pane_or_workspace_asks_when_any_view_in_it_is_modified() {
        let dir = crate::plugin::testing::tmpdir("close-pane");
        let (mut s, sid, _) = edited(&dir, "a.txt");
        s.close_focused_pane();
        assert!(s.confirm().is_some(), "closing the pane drops its modified tab");
        s.cancel_confirm();
        assert!(s.view(sid).is_some());
        s.close_focused_pane();
        s.discard_confirm().unwrap();
        assert!(s.view(sid).is_none());

        // Another pane's modified view doesn't count for this pane.
        let (mut s, sid, _) = edited(&dir, "b.txt");
        s.split_focused(Axis::LeftRight).unwrap();
        assert_ne!(s.focused_surface(), Some(sid));
        s.close_focused_pane();
        assert!(s.confirm().is_none(), "the new pane is empty");
        assert!(s.view(sid).is_some());

        // The workspace closes every pane in it, from the command or by id.
        s.split_focused(Axis::LeftRight).unwrap();
        s.close_active_vtab();
        assert!(s.confirm().is_some());
        s.discard_confirm().unwrap();
        assert_eq!(s.tree.vtabs().len(), 0);

        let (mut s, _, _) = edited(&dir, "c.txt");
        let vt = s.active().unwrap();
        s.new_empty_vtab();
        s.close_vtab(vt);
        assert!(s.confirm().is_some());
        s.cancel_confirm();
        assert_eq!(s.tree.vtabs().len(), 2);
    }

    #[test]
    fn closing_the_last_tab_of_an_empty_pane_cascades_without_asking_about_others() {
        let dir = crate::plugin::testing::tmpdir("close-cascade");
        let (mut s, sid, _) = edited(&dir, "a.txt");
        s.split_focused(Axis::LeftRight).unwrap();
        s.close_focused_surface(); // the empty split pane
        assert!(s.confirm().is_none());
        assert!(s.view(sid).is_some());
    }

    #[test]
    fn quitting_asks_over_any_workspace_and_exits_only_when_agreed() {
        let dir = crate::plugin::testing::tmpdir("quit");
        let (mut s, _, file) = edited(&dir, "a.txt");
        s.new_empty_vtab(); // the modified view is in a background workspace
        assert!(s.has_unsaved());
        s.request_quit();
        let c = s.confirm().expect("asks");
        assert_eq!(
            (c.accept, c.discard),
            (Some("Save and Quit"), Some("Quit Without Saving"))
        );
        assert!(!s.quitting());
        s.cancel_confirm();
        assert!(!s.quitting());

        s.request_quit();
        s.discard_confirm().unwrap();
        assert!(s.quitting());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old\n");

        let (mut s, _, file) = edited(&dir, "b.txt");
        s.request_quit();
        assert_eq!(s.accept_confirm().unwrap(), vec![file.clone()]);
        assert!(s.quitting());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "newold\n");
    }

    #[test]
    fn a_failed_save_keeps_the_view_open_and_does_not_quit() {
        let dir = crate::plugin::testing::tmpdir("save-fails");
        for quit in [false, true] {
            let (mut s, sid, file) = edited(&dir, if quit { "q.txt" } else { "c.txt" });
            // A read-only file makes the write fail.
            let mut perms = std::fs::metadata(&file).unwrap().permissions();
            perms.set_readonly(true);
            std::fs::set_permissions(&file, perms).unwrap();
            if quit {
                s.request_quit();
            } else {
                s.close_focused_surface();
            }
            assert!(s.accept_confirm().is_err());
            assert!(s.view(sid).is_some_and(|v| v.modified()), "still open, still unsaved");
            assert!(!s.quitting());
        }
    }

    #[test]
    fn a_modified_scratch_buffer_can_only_be_cancelled_or_discarded() {
        let mut s = AppState::new().with_unsaved_guard();
        s.new_empty_vtab();
        s.open_plugin_in_focused(editor::ID).unwrap();
        let sid = s.focused_surface().unwrap();
        s.views.get_mut(&sid).unwrap().write_input(b"x");
        s.close_focused_surface();
        let c = s.confirm().expect("asks");
        assert_eq!((c.accept, c.discard), (None, Some("Close Without Saving")));
        assert!(c.message.contains("no file"), "{}", c.message);
        assert_eq!(s.accept_confirm().unwrap(), Vec::<std::path::PathBuf>::new());
        assert!(s.confirm().is_some() && s.view(sid).is_some(), "Enter does nothing");
        s.discard_confirm().unwrap();
        assert!(s.view(sid).is_none());

        // An untouched scratch buffer closes at once.
        s.open_plugin_in_focused(editor::ID).unwrap();
        s.close_focused_surface();
        assert!(s.confirm().is_none());
    }

    #[test]
    fn the_agent_channel_closes_without_asking() {
        let dir = crate::plugin::testing::tmpdir("close-agent");
        let (mut s, sid, _) = edited(&dir, "a.txt");
        s.guard_unsaved = false;
        assert!(!s.has_unsaved(), "nothing to ask about");
        s.close_focused_surface();
        assert!(s.confirm().is_none() && s.view(sid).is_none());
    }

    #[test]
    fn asking_does_not_autosave() {
        let dir = crate::plugin::testing::tmpdir("ask-no-autosave");
        let (mut s, _, file) = edited(&dir, "a.txt");
        // Autosave on unfocus is on, but a dialog (or the picker over it) is
        // not the app losing focus: nothing writes until the user answers.
        let mut cfg = Config::default();
        cfg.editor.autosave_on_unfocus = true;
        s.close_focused_surface();
        s.open_save_as(s.focused_surface().unwrap());
        assert!(s.confirm().is_some() && s.picker_open());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old\n");
        s.cancel_confirm();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old\n");
        // Losing the window to another app still autosaves.
        assert!(!s.autosave_all(&cfg));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "newold\n");
    }
}
