# Spec.md

Technical implementation. Product shape is in [GOAL.md](GOAL.md); the libghostty
investigation and verified toolchain are in [libghostty-findings.md](libghostty-findings.md).

## Stack

- Language: Rust (workspace). Windowing/event loop: `winit`. GPU: `wgpu`.
- Terminal engine: `libghostty-vt` (VT parse + grid state + input encoding) — git
  dependency, pinned rev; see DECISION.md. It gives us cells, not pixels.
- PTY: `portable-pty`. Fonts/shaping/raster: `glyphon` (cosmic-text) for now.
- Toolchain: Zig 0.16.0 (pinned in [`../mise.toml`](../mise.toml)) is required to
  build `libghostty-vt-sys`. `.cargo/config.toml` sets `LIBGHOSTTY_VT_SYS_CPU=native`.

## Crate layout (seams)

```
crates/app         (bin "ghostrealm")  winit + wgpu scene + glyphon + input routing
  src/plugin/        the plugin API: Plugin/View, PaintCx, EventCx, test harness
  src/plugins/       first-party plugins: terminal, editor, file_browser
crates/core        app spine: command registry, tab/split tree, inbox, config, fs tree
crates/terminal    the seam: TerminalBackend trait + Grid/Cell/Cursor/KeyPress
crates/terminal-ghostty   libghostty-vt + portable-pty impl of the seam
```

- **App ↔ terminal seam:** `TerminalBackend` (in `crates/terminal`) exposes only
  backend-neutral types. Inside `terminal-ghostty`, the PTY / VT / (future) font
  pieces are each isolated so any one can be swapped as upstream Ghostty matures.
- **Rendering is the app's, not the backend's.** The backend yields a `Grid` of
  cells; the app draws it. So terminals, chrome, and the palette share one wgpu
  scene (unified compositing; no native subviews), which also keeps it
  cross-platform.
- **App ↔ content seam:** every surface's content is a plugin `View` (terminal,
  editor, file browser). The app owns layout, focus, chrome, and input routing;
  views paint immediate-mode through `PaintCx` and take input through `EventCx`.
  Contract: [PLUGINS.md](PLUGINS.md).

## Rendering model (current)

Per frame: every view is pumped (a terminal checks its VT worker's published
grid); then each pane's active view paints into a `Frame`: quads under text, text
items, quads over text, overlay text. The app draws the quads via an instanced
quad pipeline and the text via glyphon. Text is either a content-keyed shaped row
from the shared `RowCache` (shaped within a per-frame time budget) or a per-frame
scratch buffer. A terminal snapshots only when its grid changed and resizes its
PTY only when its cell grid changes.

Status/`needs_input` sourcing (planned): Enter-submission = optimistic busy;
PTY foreground process-group = authoritative busy/idle; OSC 133 = exit code;
`needs_input` via the agent back-channel.

## Verification notes

- The VT/PTY backend is covered by integration tests (grid render, SGR colour,
  exit code, key encoding).
- The wgpu render pipeline is covered by a **headless offscreen render test**
  (render to a texture, read the pixel back) because the dev sandbox has no
  display to screenshot. Run `ghostrealm` on a real desktop session to see the
  window; `ghostrealm dump [cmd]` prints a grid as text without a display.

## Status

- [x] Phase 1: libghostty verification + backend + window spike.
- [x] Phase 2: command registry + tab/split tree + command palette (Cmd+K).
- [x] Phase 3: agent control/introspection channel (`ghostrealm agent`, JSON).
- [x] Phase 4: multi-pane GUI (splits both directions), sidebar of vtabs, per-pane
      htab strips (autohide), focus routing, mouse (click vtab/pane/tab).
- [~] Phase 5: inbox status — background output → unread, sticky needs_input
      (agent/palette settable), busy from the foreground process-group (+ optimistic
      on Enter), auto-read dwell + unfocus grace. To do: OSC 133 exit code for
      unread success/failure.
- [x] Phase 6: settings TOML (load/create, wired: font, sidebar width, chrome
      colours, autohide, config-driven keybindings, sidebar `side`, chrome
      inheritance from the Ghostty config). Follow-up: resolve Ghostty themes for
      inheritance (TODO.md).

Remaining polish is tracked in [TODO.md](TODO.md). The core loop, splits, tabs,
sidebar/inbox, palette, config, and the agent channel are all in and tested.

## Verifying the GUI

The dev sandbox has no display, so `resumed` never fires there — the window,
config-file creation, and interactive paths only run on a real desktop session.
Automated coverage stands in: the workspace test suite (core logic, backend
PTY/VT, agent protocol, inbox, config, and every plugin view painted and driven
through the headless `plugin::testing::Harness`) plus an offscreen render test. Eyeball the
window by running `ghostrealm` on a real session.
