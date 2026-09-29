//! The live application state: the workspace [`Tree`] plus the terminals that
//! back its surfaces, and the command registry built over it.
//!
//! This is the single source of truth the GUI, the palette, and the agent
//! channel all act on. It is `!Send` (it owns VT engines) and lives on the UI
//! thread. Terminal grid sizes default to a headless size until the GUI drives
//! real per-pane sizes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use ghostrealm_core::{
    fs_tree::FsTree, ArgKind, ArgSpec, Axis, CmdError, CmdOutcome, CommandMeta, Inbox, PaneId,
    Registry, SurfaceId, TabStatus, Tree, VtabId,
};
use ghostrealm_terminal::{Key, KeyPress, Lifecycle, Scroll, TerminalBackend};

use crate::editor::EditorBuffer;

/// Name of the dedicated workspace the settings file opens in (shown italic).
pub const SETTINGS_VTAB_NAME: &str = "settings";

/// Reserved surface id for the floating directory picker's file browser. It lives
/// in the `browsers` map like any browser (so all browser UI code applies) but is
/// not a tree surface, so it never renders as a pane and is kept across prunes.
pub const PICKER_SID: SurfaceId = SurfaceId(u64::MAX);

/// The kinds of content a pane/tab can open (via the "nothing open" picker).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenKind {
    Terminal,
    Editor,
    Browser,
}

/// After Enter, a vtab is shown busy for at least this long even if the shell's
/// foreground process group hasn't moved yet — the command may not have forked
/// (or produced output) before the next pump. Bridges that race for silent jobs.
const OPTIMISTIC_BUSY_GRACE: Duration = Duration::from_millis(600);
use ghostrealm_terminal_ghostty::{CommandBuilder, ThreadedTerminal};

/// Default grid size for a surface before the GUI assigns it a pane rect.
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_CELL_W: u32 = 8;
const DEFAULT_CELL_H: u32 = 16;

pub struct AppState {
    pub tree: Tree,
    surfaces: HashMap<SurfaceId, ThreadedTerminal>,
    /// Surfaces whose content is a text editor rather than a terminal.
    editors: HashMap<SurfaceId, EditorBuffer>,
    /// Surfaces whose content is a file browser. A surface in none of the three
    /// maps is "empty" — it shows the open-something picker.
    browsers: HashMap<SurfaceId, FsTree>,
    /// Optional shell command line (`sh -c <line>`); `None` = the user's shell.
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
            surfaces: HashMap::new(),
            editors: HashMap::new(),
            browsers: HashMap::new(),
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

    /// Spawn a surface's terminal in workspace `vt`'s resolved cwd.
    fn spawn_surface(&mut self, id: SurfaceId, vt: VtabId) -> Result<()> {
        let cwd = self.resolve_cwd(vt);
        self.spawn_surface_cmd(id, None, cwd)
    }

    /// Spawn a surface's terminal. `cmd_line`, when given, runs `sh -c <cmd_line>`
    /// for this surface only; otherwise the configured shell line (or login shell)
    /// is used. `cwd`, when it exists, is the working directory.
    fn spawn_surface_cmd(
        &mut self,
        id: SurfaceId,
        cmd_line: Option<&str>,
        cwd: Option<std::path::PathBuf>,
    ) -> Result<()> {
        let mut cmd = match cmd_line.or(self.shell_line.as_deref()) {
            Some(line) => {
                let mut c = CommandBuilder::new("/bin/sh");
                c.arg("-c");
                c.arg(line);
                c
            }
            None => CommandBuilder::new_default_prog(),
        };
        if let Some(dir) = cwd.filter(|d| d.is_dir()) {
            cmd.cwd(dir);
        }
        // The VT runs on a worker thread; the UI waker pokes the event loop when
        // the worker publishes a new grid.
        let term = ThreadedTerminal::spawn(
            DEFAULT_COLS,
            DEFAULT_ROWS,
            DEFAULT_CELL_W,
            DEFAULT_CELL_H,
            Some(cmd),
            self.waker.clone(),
        )?;
        self.surfaces.insert(id, term);
        Ok(())
    }

    pub fn terminal(&mut self, id: SurfaceId) -> Option<&mut ThreadedTerminal> {
        self.surfaces.get_mut(&id)
    }

    /// Create a new vtab with a spawned terminal; it becomes active.
    pub fn new_vtab(&mut self) -> Result<VtabId> {
        let name = format!("tab {}", self.next_tab_number);
        self.next_tab_number += 1;
        let (vt, _pane, surf) = self.tree.add_vtab(name);
        self.spawn_surface(surf, vt)?;
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
        self.spawn_surface(surf, vt)?; // the user's interactive shell, not `sh -c`
        // Feed the command to the live shell (it echoes + runs it, then stays
        // interactive). The PTY buffers this until the shell is ready to read.
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        if let Some(t) = self.surfaces.get_mut(&surf) {
            t.write_bytes(&bytes);
        }
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
        if let Some((_new_pane, surf)) = self.tree.split(vt, pane, axis) {
            self.spawn_surface(surf, vt)?;
        }
        Ok(())
    }

    pub fn new_surface_in_focused(&mut self) -> Result<()> {
        let Some((vt, pane)) = self.focused_pane() else {
            return Ok(());
        };
        if let Some(surf) = self.tree.add_surface(vt, pane) {
            self.spawn_surface(surf, vt)?;
        }
        Ok(())
    }

    /// Add an editor surface (a text buffer, not a terminal) to the focused pane
    /// and make it active. `buffer` is a scratch or file-backed [`EditorBuffer`].
    pub fn open_editor_in_focused(&mut self, buffer: EditorBuffer) {
        let Some((vt, pane)) = self.focused_pane() else {
            return;
        };
        if let Some(surf) = self.tree.add_surface(vt, pane) {
            self.tree.set_surface_title(surf, buffer.title(), true);
            self.editors.insert(surf, buffer);
        }
    }

    /// Add an empty surface (a new tab with no content) to the focused pane and
    /// make it active. It shows the open-something picker until a kind is chosen.
    pub fn add_empty_surface_in_focused(&mut self) {
        if let Some((vt, pane)) = self.focused_pane() {
            self.tree.add_surface(vt, pane);
        }
    }

    /// Open `kind` in the focused pane: fill the active empty slot in place when
    /// there is one (the picker), otherwise add a new tab of that kind.
    pub fn open_kind_in_focused(&mut self, kind: OpenKind) -> Result<()> {
        let Some((vt, pane)) = self.focused_pane() else {
            return Ok(());
        };
        let target = match self.focused_surface() {
            Some(sid) if self.surface_is_empty(sid) => Some(sid),
            _ => self.tree.add_surface(vt, pane),
        };
        if let Some(sid) = target {
            self.materialize_surface(sid, vt, kind)?;
        }
        Ok(())
    }

    /// Give an existing (empty) surface content of `kind`.
    fn materialize_surface(&mut self, sid: SurfaceId, vt: VtabId, kind: OpenKind) -> Result<()> {
        match kind {
            OpenKind::Terminal => self.spawn_surface(sid, vt)?,
            OpenKind::Editor => {
                let buf = EditorBuffer::scratch();
                self.tree.set_surface_title(sid, buf.title(), true);
                self.editors.insert(sid, buf);
            }
            OpenKind::Browser => {
                let root = self
                    .resolve_cwd(vt)
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                self.tree.set_surface_title(sid, "Files", true);
                self.browsers.insert(sid, FsTree::new(root));
            }
        }
        Ok(())
    }

    /// Whether `id` is an empty surface (no terminal, editor, or browser yet).
    pub fn surface_is_empty(&self, id: SurfaceId) -> bool {
        !self.surfaces.contains_key(&id)
            && !self.editors.contains_key(&id)
            && !self.browsers.contains_key(&id)
    }

    /// Whether `id` is a file-browser surface.
    pub fn is_browser(&self, id: SurfaceId) -> bool {
        self.browsers.contains_key(&id)
    }

    /// The file-browser model for `id`, if it is a browser surface.
    pub fn browser(&self, id: SurfaceId) -> Option<&FsTree> {
        self.browsers.get(&id)
    }

    pub fn browser_mut(&mut self, id: SurfaceId) -> Option<&mut FsTree> {
        self.browsers.get_mut(&id)
    }

    /// Whether the focused pane's active surface is a file browser.
    pub fn focused_is_browser(&self) -> bool {
        self.focused_surface()
            .map(|s| self.is_browser(s))
            .unwrap_or(false)
    }

    /// Open `path` in a new editor tab in the focused pane (from the browser).
    pub fn open_file_in_focused(&mut self, path: std::path::PathBuf) {
        self.open_editor_in_focused(EditorBuffer::open(path));
    }

    /// Whether the floating directory picker is open.
    pub fn dir_picker_open(&self) -> bool {
        self.browsers.contains_key(&PICKER_SID)
    }

    /// Open the floating directory picker rooted at the active workspace's dir
    /// (else the app default, else the current dir).
    pub fn open_dir_picker(&mut self) {
        let root = self
            .active()
            .and_then(|vt| self.resolve_cwd(vt))
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        self.browsers.insert(PICKER_SID, FsTree::new(root));
    }

    /// Close the floating directory picker.
    pub fn close_dir_picker(&mut self) {
        self.browsers.remove(&PICKER_SID);
    }

    /// Confirm the picker: pin the active workspace's root to the picker's current
    /// directory, then close it.
    pub fn confirm_dir_picker(&mut self) {
        if let Some(root) = self.browsers.get(&PICKER_SID).map(|b| b.root().to_path_buf()) {
            self.set_active_root_dir(root);
        }
        self.close_dir_picker();
    }

    /// Create a new workspace whose sole surface is an editor (no shell spawned),
    /// named `name` (pinned). Returns the new vtab id.
    pub fn new_editor_vtab(&mut self, name: impl Into<String>, buffer: EditorBuffer) -> VtabId {
        let (vt, _pane, surf) = self.tree.add_vtab(name);
        if let Some(v) = self.tree.vtab_mut(vt) {
            v.user_named = true;
        }
        self.tree.set_surface_title(surf, buffer.title(), true);
        self.editors.insert(surf, buffer);
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
                            .any(|s| self.editors.contains_key(&s.id))
                    })
                    .unwrap_or(false);
                if !has_editor {
                    self.open_editor_in_focused(EditorBuffer::open(path));
                }
            }
            None => {
                let vt = self.new_editor_vtab(SETTINGS_VTAB_NAME, EditorBuffer::open(path));
                self.focus_vtab(vt);
            }
        }
    }

    /// Whether `id` is an editor surface.
    pub fn is_editor(&self, id: SurfaceId) -> bool {
        self.editors.contains_key(&id)
    }

    /// The editor buffer for `id`, if it is an editor surface.
    pub fn editor(&self, id: SurfaceId) -> Option<&EditorBuffer> {
        self.editors.get(&id)
    }

    pub fn editor_mut(&mut self, id: SurfaceId) -> Option<&mut EditorBuffer> {
        self.editors.get_mut(&id)
    }

    /// Save every editor with unsaved edits and a backing file (autosave on app
    /// blur/quit). Pathless scratch buffers are left alone. Returns whether the
    /// config file was among those saved (so the caller can hot-reload it).
    pub fn autosave_all_editors(&mut self) -> bool {
        let config_path = ghostrealm_core::config::config_path();
        let mut saved_config = false;
        for e in self.editors.values_mut() {
            if e.modified && e.path.is_some() {
                let _ = e.save();
                if config_path.is_some() && e.path == config_path {
                    saved_config = true;
                }
            }
        }
        saved_config
    }

    /// Whether the focused surface is an editor.
    pub fn focused_is_editor(&self) -> bool {
        self.focused_surface()
            .map(|id| self.is_editor(id))
            .unwrap_or(false)
    }

    /// The focused surface's editor buffer, if it is an editor.
    pub fn focused_editor_mut(&mut self) -> Option<&mut EditorBuffer> {
        let id = self.focused_surface()?;
        self.editors.get_mut(&id)
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
    /// the workspace on its empty screen. Invoked on that empty screen, it closes
    /// the workspace itself.
    pub fn close_focused_surface(&mut self) {
        let Some((vt, pane)) = self.focused_pane() else {
            return;
        };
        match self.focused_surface() {
            Some(sid) => self.prune_orphan_surfaces_after(|s| s.tree.close_surface(vt, pane, sid)),
            // Empty pane (nothing open): close the workspace.
            None => self.prune_orphan_surfaces_after(|s| s.tree.close_vtab(vt)),
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
        self.surfaces.retain(|id, _| live.contains(id));
        self.editors.retain(|id, _| live.contains(id));
        // The picker browser is not a tree surface; keep it while it is open.
        self.browsers
            .retain(|id, _| live.contains(id) || *id == PICKER_SID);
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

    /// Write raw bytes to a surface's terminal (agent-driven input).
    pub fn write_input(&mut self, id: SurfaceId, bytes: &[u8]) -> bool {
        match self.surfaces.get_mut(&id) {
            Some(t) => {
                t.write_bytes(bytes);
                true
            }
            None => false,
        }
    }

    /// Encode and send a key press to the focused surface. Input snaps the
    /// viewport back to the live bottom, as terminals do. Enter optimistically
    /// marks the active vtab busy so submitting a command reacts instantly, even
    /// before its foreground process group appears.
    pub fn send_key_to_focused(&mut self, press: &KeyPress) -> bool {
        let sent = match self
            .focused_surface()
            .and_then(|id| self.surfaces.get_mut(&id))
        {
            Some(t) => {
                t.scroll(Scroll::Bottom);
                t.send_key(press);
                true
            }
            None => false,
        };
        if sent && press.key == Key::Enter {
            if let Some(vt) = self.active() {
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
        }
        sent
    }

    /// Write raw bytes to the focused surface (e.g. a paste or a line-editing
    /// escape sequence), snapping the viewport to the bottom first.
    pub fn write_to_focused(&mut self, bytes: &[u8]) -> bool {
        match self
            .focused_surface()
            .and_then(|id| self.surfaces.get_mut(&id))
        {
            Some(t) => {
                t.scroll(Scroll::Bottom);
                t.write_bytes(bytes);
                true
            }
            None => false,
        }
    }

    /// Scroll the focused surface's scrollback viewport.
    pub fn scroll_focused(&mut self, scroll: Scroll) -> bool {
        match self
            .focused_surface()
            .and_then(|id| self.surfaces.get_mut(&id))
        {
            Some(t) => {
                t.scroll(scroll);
                true
            }
            None => false,
        }
    }

    /// Resize a surface's terminal to a pane's grid dimensions.
    pub fn resize_surface(
        &mut self,
        id: SurfaceId,
        cols: u16,
        rows: u16,
        cell_w: u32,
        cell_h: u32,
    ) {
        if let Some(t) = self.surfaces.get_mut(&id) {
            t.resize(cols, rows, cell_w, cell_h);
        }
    }

    /// Pump all terminals and sync program-set surface titles into the tree.
    /// Returns true if any terminal produced output (the grid may have changed).
    pub fn pump_all(&mut self) -> bool {
        self.pump_all_budgeted(usize::MAX).0
    }

    /// Pump all terminals, bounding each to roughly `budget` bytes of output so a
    /// flood in one surface cannot monopolise a frame. Returns
    /// `(changed, more_pending)`: `more_pending` means at least one surface still
    /// has queued output and should be pumped again promptly.
    pub fn pump_all_budgeted(&mut self, budget: usize) -> (bool, bool) {
        let active = self.tree.active_vtab();
        let mut changed = false;
        let mut more = false;
        let mut titles: Vec<(SurfaceId, String)> = Vec::new();
        let mut changed_surfaces: Vec<SurfaceId> = Vec::new();
        for (id, term) in self.surfaces.iter_mut() {
            let pumped = term.pump_budgeted(budget);
            if pumped.changed {
                changed = true;
                changed_surfaces.push(*id);
            }
            more |= pumped.more;
            if let Some(t) = term.title() {
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
                        self.surfaces
                            .get(&s.id)
                            .map(|t| t.is_busy())
                            .unwrap_or(false)
                    })
            })
            .unwrap_or(false)
    }

    /// Whether a live surface (terminal or editor) exists for `id`.
    pub fn has_surface(&self, id: SurfaceId) -> bool {
        self.surfaces.contains_key(&id) || self.editors.contains_key(&id)
    }

    /// Whether a surface's grid changed since it was last snapshotted.
    pub fn surface_needs_snapshot(&self, id: SurfaceId) -> bool {
        self.surfaces
            .get(&id)
            .map(|t| t.needs_snapshot())
            .unwrap_or(false)
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

    /// A text rendering of one surface's grid (pumps it first).
    pub fn surface_text(&mut self, id: SurfaceId) -> Option<String> {
        let term = self.surfaces.get_mut(&id)?;
        term.pump();
        let grid = term.snapshot();
        let mut out = String::new();
        for row in 0..grid.size.rows {
            let mut line = String::new();
            for col in 0..grid.size.cols {
                match grid.cell(col, row) {
                    Some(c) if !c.text.is_empty() => line.push_str(&c.text),
                    _ => line.push(' '),
                }
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        Some(out)
    }

    pub fn surface_lifecycle(&mut self, id: SurfaceId) -> Option<Lifecycle> {
        self.surfaces.get_mut(&id).map(|t| t.lifecycle())
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
        CommandMeta::new("tab.new", "New Workspace", "Open a new empty workspace"),
        Box::new(|s: &mut AppState, _| {
            let id = s.new_empty_vtab();
            Ok(CmdOutcome::msg(format!("opened workspace {}", id.0)))
        }),
    );
    r.register(
        CommandMeta::new("tab.close", "Close Workspace", "Close the active workspace"),
        Box::new(|s: &mut AppState, _| {
            s.close_active_vtab();
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new("tab.rename", "Rename Workspace", "Rename the active workspace")
            .arg(ArgSpec::required("name", ArgKind::Str, "the new workspace name")),
        Box::new(|s: &mut AppState, a| {
            s.rename_active_vtab(a.get_str("name")?);
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "workspace.set_root",
            "Set Workspace Directory",
            "Pin the active workspace's root directory (new terminals start here)",
        )
        .arg(ArgSpec::required(
            "path",
            ArgKind::Str,
            "the directory to pin",
        )),
        Box::new(|s: &mut AppState, a| {
            if let Some(dir) = ghostrealm_core::config::expand_tilde(a.get_str("path")?) {
                s.set_active_root_dir(dir);
            }
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "workspace.pick_dir",
            "Set Workspace Directory…",
            "Choose the active workspace's directory in a floating file picker",
        ),
        Box::new(|s: &mut AppState, _| {
            s.open_dir_picker();
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
            "terminal.new",
            "New Terminal",
            "Open a terminal in the focused pane",
        ),
        Box::new(|s: &mut AppState, _| {
            s.open_kind_in_focused(OpenKind::Terminal).map_err(failed)?;
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "editor.scratch",
            "New Editor",
            "Open an empty text editor in the focused pane",
        ),
        Box::new(|s: &mut AppState, _| {
            s.open_kind_in_focused(OpenKind::Editor).map_err(failed)?;
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "browser.new",
            "New File Browser",
            "Open a file browser in the focused pane",
        ),
        Box::new(|s: &mut AppState, _| {
            s.open_kind_in_focused(OpenKind::Browser).map_err(failed)?;
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "editor.open",
            "Open File in Editor",
            "Open a file in a text editor in the focused pane",
        )
        .arg(ArgSpec::required("path", ArgKind::Str, "the file path to open")),
        Box::new(|s: &mut AppState, a| {
            let path = std::path::PathBuf::from(a.get_str("path")?);
            s.open_editor_in_focused(EditorBuffer::open(path));
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "tab.mark_read",
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
            "tab.mark_unread",
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
            "tab.needs_input",
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
            "tab.dismiss",
            "Dismiss Workspace Status",
            "Clear a sticky needs-input flag",
        ),
        Box::new(|s: &mut AppState, _| {
            s.set_active_status(TabStatus::Read);
            Ok(CmdOutcome::ok())
        }),
    );

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
            s.terminal(surf).is_some(),
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
    fn split_spawns_a_second_terminal() {
        let mut s = AppState::new().with_shell_line("sleep 1");
        s.new_vtab().unwrap();
        s.split_focused(Axis::LeftRight).unwrap();
        let vt = s.tree.active_vtab().unwrap();
        assert_eq!(s.tree.vtab(vt).unwrap().panes().len(), 2);
        // Both surfaces should have live terminals.
        let ids: Vec<_> = s
            .tree
            .vtab(vt)
            .unwrap()
            .panes()
            .iter()
            .map(|p| p.surfaces[0].id)
            .collect();
        for id in ids {
            assert!(
                s.terminal(id).is_some(),
                "each split pane should have a terminal"
            );
        }
    }

    #[test]
    fn closing_focused_pane_drops_its_terminal() {
        let mut s = AppState::new().with_shell_line("sleep 1");
        s.new_vtab().unwrap();
        s.split_focused(Axis::TopBottom).unwrap();
        let before = s.tree.surface_count();
        s.close_focused_pane();
        let after = s.tree.surface_count();
        assert_eq!(after, before - 1, "closing a pane should drop one surface");
        assert_eq!(
            s.surfaces.len(),
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
        assert_eq!(s.surfaces.len(), s.tree.surface_count());

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

        // The focused pane has one surface; closing it collapses the split back
        // to a single pane without closing the workspace.
        s.close_focused_surface();
        assert_eq!(s.tree.vtab(vt).unwrap().panes().len(), 1);
        assert!(s.tree.vtab(vt).is_some(), "workspace should survive");
        assert_eq!(s.surfaces.len(), s.tree.surface_count());
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
        s.open_editor_in_focused(EditorBuffer::scratch());

        assert!(s.focused_is_editor(), "the opened editor becomes focused");
        let surf = s.focused_surface().unwrap();
        assert!(s.is_editor(surf));
        assert!(s.editor(surf).is_some());
        assert!(
            s.terminal(surf).is_none(),
            "an editor surface has no terminal"
        );

        // Typing routes into the buffer.
        s.focused_editor_mut().unwrap().insert_char('x');
        assert_eq!(s.editor(surf).unwrap().lines, vec!["x".to_string()]);
    }

    #[test]
    fn close_vtab_by_id_removes_it_and_prunes_its_surface() {
        let mut s = AppState::new().with_shell_line("sleep 2");
        let a = s.new_vtab().unwrap();
        let b = s.new_vtab().unwrap();
        let before = s.tree.vtabs().len();
        let surfaces_before = s.surfaces.len();

        s.close_vtab(a);

        assert_eq!(s.tree.vtabs().len(), before - 1);
        assert!(s.tree.vtab(a).is_none(), "the closed vtab is gone");
        assert!(s.tree.vtab(b).is_some(), "other vtabs remain");
        assert_eq!(
            s.surfaces.len(),
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
        r.execute("tab.mark_unread", &ghostrealm_core::Args::new(), &mut s)
            .expect("tab.mark_unread runs");
        assert!(
            matches!(s.tree.vtab(vt).unwrap().status, TabStatus::Unread { .. }),
            "tab.mark_unread should set the active tab unread"
        );
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
