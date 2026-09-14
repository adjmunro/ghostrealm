//! The live application state: the workspace [`Tree`] plus the terminals that
//! back its surfaces, and the command registry built over it.
//!
//! This is the single source of truth the GUI, the palette, and the agent
//! channel all act on. It is `!Send` (it owns VT engines) and lives on the UI
//! thread. Terminal grid sizes default to a headless size until the GUI drives
//! real per-pane sizes.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use ghostrealm_core::{
    ArgKind, ArgSpec, Axis, CmdError, CmdOutcome, CommandMeta, Registry, SurfaceId, TabStatus,
    Tree, VtabId,
};
use ghostrealm_terminal::{KeyPress, Lifecycle, Scroll, TerminalBackend};
use ghostrealm_terminal_ghostty::{CommandBuilder, GhosttyTerminal, PtyWaker};

/// Default grid size for a surface before the GUI assigns it a pane rect.
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_CELL_W: u32 = 8;
const DEFAULT_CELL_H: u32 = 16;

pub struct AppState {
    pub tree: Tree,
    surfaces: HashMap<SurfaceId, GhosttyTerminal>,
    /// Optional shell command line (`sh -c <line>`); `None` = the user's shell.
    shell_line: Option<String>,
    /// Shared source for per-surface PTY wakers (the GUI wires this to its event
    /// loop). Cloned into a fresh `PtyWaker` for each spawned terminal.
    waker: Option<Arc<dyn Fn() + Send + Sync>>,
    next_tab_number: u32,
}

impl AppState {
    pub fn new() -> Self {
        AppState {
            tree: Tree::new(),
            surfaces: HashMap::new(),
            shell_line: None,
            waker: None,
            next_tab_number: 1,
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

    fn spawn_surface(&mut self, id: SurfaceId) -> Result<()> {
        self.spawn_surface_cmd(id, None)
    }

    /// Spawn a surface's terminal. `cmd_line`, when given, runs `sh -c <cmd_line>`
    /// for this surface only; otherwise the configured shell line (or login shell)
    /// is used.
    fn spawn_surface_cmd(&mut self, id: SurfaceId, cmd_line: Option<&str>) -> Result<()> {
        let line = cmd_line.or(self.shell_line.as_deref());
        let cmd = line.map(|line| {
            let mut c = CommandBuilder::new("/bin/sh");
            c.arg("-c");
            c.arg(line);
            c
        });
        let waker: Option<PtyWaker> = self.waker.clone().map(|src| {
            let w: PtyWaker = Box::new(move || src());
            w
        });
        let term = GhosttyTerminal::spawn_with_waker(
            DEFAULT_COLS,
            DEFAULT_ROWS,
            DEFAULT_CELL_W,
            DEFAULT_CELL_H,
            cmd,
            waker,
        )?;
        self.surfaces.insert(id, term);
        Ok(())
    }

    pub fn terminal(&mut self, id: SurfaceId) -> Option<&mut GhosttyTerminal> {
        self.surfaces.get_mut(&id)
    }

    /// Create a new vtab with a spawned terminal; it becomes active.
    pub fn new_vtab(&mut self) -> Result<VtabId> {
        let name = format!("tab {}", self.next_tab_number);
        self.next_tab_number += 1;
        let (vt, _pane, surf) = self.tree.add_vtab(name);
        self.spawn_surface(surf)?;
        Ok(vt)
    }

    /// Create a new vtab that runs `command_line` in a fresh shell ("Run
    /// Anything"). The vtab is named after the command until the program sets its
    /// own title; it becomes active.
    pub fn new_vtab_running(&mut self, command_line: impl Into<String>) -> Result<VtabId> {
        let line = command_line.into();
        let name = line.split_whitespace().next().unwrap_or("run").to_string();
        self.next_tab_number += 1;
        let (vt, _pane, surf) = self.tree.add_vtab(name);
        self.spawn_surface_cmd(surf, Some(&line))?;
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
            self.spawn_surface(surf)?;
        }
        Ok(())
    }

    pub fn new_surface_in_focused(&mut self) -> Result<()> {
        let Some((vt, pane)) = self.focused_pane() else {
            return Ok(());
        };
        if let Some(surf) = self.tree.add_surface(vt, pane) {
            self.spawn_surface(surf)?;
        }
        Ok(())
    }

    /// Cycle focus to the next pane in the active vtab.
    pub fn focus_next_pane(&mut self) {
        let Some(vt) = self.active() else { return };
        let Some(v) = self.tree.vtab(vt) else { return };
        let panes: Vec<_> = v.panes().iter().map(|p| p.id).collect();
        if panes.len() < 2 {
            return;
        }
        let cur = v.focused_pane;
        let pos = panes.iter().position(|&p| p == cur).unwrap_or(0);
        let next = panes[(pos + 1) % panes.len()];
        if let Some(v) = self.tree.vtab_mut(vt) {
            v.focused_pane = next;
        }
    }

    pub fn close_focused_pane(&mut self) {
        if let Some((vt, pane)) = self.focused_pane() {
            self.prune_orphan_surfaces_after(|s| s.tree.close_pane(vt, pane));
        }
    }

    pub fn close_active_vtab(&mut self) {
        if let Some(vt) = self.active() {
            self.prune_orphan_surfaces_after(|s| s.tree.close_vtab(vt));
        }
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
    }

    pub fn rename_active_vtab(&mut self, name: impl Into<String>) {
        if let Some(vt) = self.active() {
            if let Some(v) = self.tree.vtab_mut(vt) {
                v.name = name.into();
                v.user_named = true;
            }
        }
    }

    /// Focus a vtab, clearing an `unread` status to `read` (a sticky
    /// `needs_input` is preserved).
    pub fn focus_vtab(&mut self, id: VtabId) {
        if !self.tree.focus_vtab(id) {
            return;
        }
        let clear = self
            .tree
            .vtab(id)
            .map(|v| matches!(v.status, TabStatus::Unread { .. }))
            .unwrap_or(false);
        if clear {
            self.tree.set_status(id, TabStatus::Read);
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
    /// viewport back to the live bottom, as terminals do.
    pub fn send_key_to_focused(&mut self, press: &KeyPress) -> bool {
        match self
            .focused_surface()
            .and_then(|id| self.surfaces.get_mut(&id))
        {
            Some(t) => {
                t.scroll(Scroll::Bottom);
                t.send_key(press);
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
        (changed, more)
    }

    /// Whether a live terminal exists for `id`.
    pub fn has_surface(&self, id: SurfaceId) -> bool {
        self.surfaces.contains_key(&id)
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
        CommandMeta::new("tab.new", "New Tab", "Open a new vertical tab"),
        Box::new(|s: &mut AppState, _| {
            let id = s.new_vtab().map_err(failed)?;
            Ok(CmdOutcome::msg(format!("opened vtab {}", id.0)))
        }),
    );
    r.register(
        CommandMeta::new("tab.close", "Close Tab", "Close the active vertical tab"),
        Box::new(|s: &mut AppState, _| {
            s.close_active_vtab();
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new("tab.rename", "Rename Tab", "Rename the active vertical tab")
            .arg(ArgSpec::required("name", ArgKind::Str, "the new tab name")),
        Box::new(|s: &mut AppState, a| {
            s.rename_active_vtab(a.get_str("name")?);
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
        CommandMeta::new("pane.close", "Close Pane", "Close the focused pane"),
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
            "surface.new",
            "New Terminal Tab",
            "Add a terminal tab to the focused pane",
        ),
        Box::new(|s: &mut AppState, _| {
            s.new_surface_in_focused().map_err(failed)?;
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "tab.mark_read",
            "Mark Tab Read",
            "Clear the active tab's inbox status",
        ),
        Box::new(|s: &mut AppState, _| {
            s.set_active_status(TabStatus::Read);
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "tab.needs_input",
            "Mark Tab Needs Input",
            "Flag the active tab as blocked awaiting the user (agent self-report)",
        ),
        Box::new(|s: &mut AppState, _| {
            s.set_active_status(TabStatus::NeedsInput);
            Ok(CmdOutcome::ok())
        }),
    );
    r.register(
        CommandMeta::new(
            "tab.dismiss",
            "Dismiss Tab Status",
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
    fn new_vtab_running_executes_its_command() {
        // The run-anything path spawns a vtab whose surface runs the given command,
        // independent of the app's configured shell line.
        let mut s = AppState::new().with_shell_line("sleep 5");
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
    fn background_output_marks_unread_and_focus_clears() {
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
        // Focusing b clears it.
        s.focus_vtab(b);
        assert_eq!(s.tree.vtab(b).unwrap().status, TabStatus::Read);
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
