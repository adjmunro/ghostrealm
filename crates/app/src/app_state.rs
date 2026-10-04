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
    ArgKind, ArgSpec, Axis, CmdError, CmdOutcome, CommandMeta, Inbox, PaneId, Registry, SurfaceId,
    TabStatus, Tree, VtabId,
};
use ghostrealm_core::Config;
use ghostrealm_terminal::{KeyPress, Lifecycle};

use crate::plugin::{EventCx, OpenCx, Plugin, Request, UiMetrics, View};
use crate::plugins::editor::{self, EditorBuffer, EditorView};
use crate::plugins::file_browser::FileBrowserView;
use crate::plugins::terminal::{self, TerminalView};

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

/// What confirming the floating picker does with its directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Pin the active workspace's root.
    ActiveRoot,
    /// Create a new workspace rooted there.
    NewWorkspace,
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
    /// Optional command line new terminals run (`sh -c <line>`); `None` = the
    /// user's shell.
    shell_line: Option<String>,
    /// Shared source for per-surface PTY wakers (the GUI wires this to its event
    /// loop). Cloned into a fresh `PtyWaker` for each spawned terminal.
    waker: Option<Arc<dyn Fn() + Send + Sync>>,
    next_tab_number: u32,
    /// Per-vtab deadline until which an optimistic (post-Enter) busy status holds
    /// even if the foreground process group hasn't moved yet.
    optimistic_busy: HashMap<VtabId, Instant>,
    /// Inbox timing config (auto-read dwell + unfocus grace).
    inbox: Inbox,
    /// Base directory new workspaces start in when they have no pinned root.
    default_dir: Option<std::path::PathBuf>,
    /// When the active vtab was focused, for the auto-read dwell timer.
    focus_started: Instant,
    /// The vtab most recently auto-read by dwell, and when — for the unfocus grace
    /// (leaving too soon after an auto-read reverts it to unread).
    last_auto_read: Option<(VtabId, Instant)>,
}

impl AppState {
    pub fn new() -> Self {
        AppState {
            tree: Tree::new(),
            views: HashMap::new(),
            plugins: crate::plugins::builtin(),
            picker: None,
            shell_line: None,
            waker: None,
            next_tab_number: 1,
            optimistic_busy: HashMap::new(),
            inbox: Inbox::default(),
            default_dir: None,
            focus_started: Instant::now(),
            last_auto_read: None,
        }
    }

    /// Set the inbox timing config (auto-read dwell + unfocus grace).
    pub fn set_inbox_config(&mut self, inbox: Inbox) {
        self.inbox = inbox;
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
        self.waker = Some(f);
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
        let cx = OpenCx {
            cwd: self.resolve_cwd(vt),
            command: self.shell_line.clone(),
            waker: self.waker.clone(),
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
        let Some((vt, pane)) = self.focused_pane() else {
            return Ok(());
        };
        let target = match self.focused_surface() {
            Some(sid) if self.surface_is_empty(sid) => Some(sid),
            _ => self.tree.add_surface(vt, pane),
        };
        if let Some(sid) = target {
            self.materialize_plugin(sid, vt, id)?;
        }
        Ok(())
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
    pub fn picker_mut(&mut self) -> Option<&mut FileBrowserView> {
        self.picker.as_mut().map(|(b, _)| b)
    }

    /// What confirming the open picker does.
    pub fn picker_purpose(&self) -> Option<Pick> {
        self.picker.as_ref().map(|(_, p)| *p)
    }

    /// Open the floating picker for `purpose`, rooted at the active
    /// workspace's dir (else the app default, else the current dir).
    pub fn open_picker(&mut self, purpose: Pick) {
        let root = self
            .active()
            .and_then(|vt| self.resolve_cwd(vt))
            .or_else(|| self.default_dir.clone())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        self.picker = Some((FileBrowserView::picker(root), purpose));
    }

    /// Close the floating picker.
    pub fn close_picker(&mut self) {
        self.picker = None;
    }

    /// Confirm the picker with its chosen directory, then close it.
    pub fn confirm_picker(&mut self) {
        if let Some((browser, purpose)) = self.picker.take() {
            let dir = browser.choice();
            match purpose {
                Pick::ActiveRoot => self.set_active_root_dir(dir),
                Pick::NewWorkspace => {
                    self.new_workspace_in(dir);
                }
            }
        }
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

    /// Open the settings file in a dedicated "settings" workspace: focus the
    /// existing one (opening the editor there if it was closed), or create it.
    pub fn open_settings(&mut self, path: std::path::PathBuf) {
        let existing = self
            .tree
            .vtabs()
            .iter()
            .find(|v| v.name == SETTINGS_VTAB_NAME)
            .map(|v| v.id);
        match existing {
            Some(vt) => {
                self.focus_vtab(vt);
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
                if !has_editor {
                    self.open_view_in_focused(Box::new(EditorView::new(EditorBuffer::open(path))));
                }
            }
            None => {
                let view = Box::new(EditorView::new(EditorBuffer::open(path)));
                let vt = self.new_view_vtab(SETTINGS_VTAB_NAME, view);
                self.focus_vtab(vt);
            }
        }
    }

    /// Persist every view's unsaved state that has a home (autosave on app
    /// blur/quit). Returns whether the config file was among what was saved (so
    /// the caller can hot-reload it).
    pub fn autosave_all(&mut self) -> bool {
        let config_path = ghostrealm_core::config::config_path();
        let mut saved_config = false;
        for view in self.views.values_mut() {
            if let Some(path) = view.autosave() {
                saved_config |= Some(path) == config_path;
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
            self.prune_orphan_surfaces_after(|s| s.tree.close_pane(vt, pane));
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
            self.prune_orphan_surfaces_after(|s| s.tree.close_surface(vt, pane, sid));
            return;
        }
        // Empty pane: collapse the split if there are siblings, else close the vtab.
        let sole = self.tree.vtab(vt).map(|v| v.panes().len() <= 1).unwrap_or(true);
        if sole {
            self.prune_orphan_surfaces_after(|s| s.tree.close_vtab(vt));
        } else {
            self.prune_orphan_surfaces_after(|s| s.tree.close_pane(vt, pane));
        }
    }

    pub fn close_active_vtab(&mut self) {
        if let Some(vt) = self.active() {
            self.prune_orphan_surfaces_after(|s| s.tree.close_vtab(vt));
        }
    }

    /// Close a specific vtab by id (e.g. from the sidebar context menu).
    pub fn close_vtab(&mut self, id: VtabId) {
        self.prune_orphan_surfaces_after(|s| s.tree.close_vtab(id));
    }

    /// Close one surface (htab) within a pane; the tree collapses the pane, and
    /// then the vtab, if that was its last surface.
    pub fn close_surface(&mut self, vtab: VtabId, pane: PaneId, surface: SurfaceId) {
        self.prune_orphan_surfaces_after(|s| s.tree.close_surface(vtab, pane, surface));
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
            if req == Request::Busy {
                self.mark_busy(sid);
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
        let mut changed = false;
        let mut more = false;
        let mut titles: Vec<(SurfaceId, String)> = Vec::new();
        let mut changed_surfaces: Vec<SurfaceId> = Vec::new();
        for (id, view) in self.views.iter_mut() {
            let pumped = view.pump(budget);
            if pumped.changed {
                changed = true;
                changed_surfaces.push(*id);
            }
            more |= pumped.more;
            if let Some(t) = view.title() {
                titles.push((*id, t));
            }
        }
        for (id, title) in titles {
            self.tree.set_surface_title(id, title, false);
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

    for plugin in crate::plugins::builtin() {
        plugin.register_commands(&mut r);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

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
        s.confirm_picker();
        assert!(!s.picker_open());
        assert_eq!(s.tree.vtab(vt).unwrap().root_dir.as_deref(), Some(sub.as_path()));
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
        s.confirm_picker();
        assert_eq!(count(&s), 1);
        let vt = s.active().unwrap();
        assert_eq!(s.tree.vtab(vt).unwrap().root_dir.as_deref(), Some(proj.as_path()));

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
}
