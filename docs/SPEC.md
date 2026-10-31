# Spec.md

Technical implementation. Product shape is in [GOAL.md](GOAL.md); the libghostty
investigation and verified toolchain are in [libghostty-findings.md](libghostty-findings.md).

## Stack

- Language: Rust (workspace). Windowing/event loop: `winit`. GPU: `wgpu`.
- Terminal engine: `libghostty-vt` (VT parse + grid state + input encoding) — git
  dependency, pinned rev; see DECISION.md. It gives us cells, not pixels.
- PTY: own unix code (`posix_openpt`, std `Command`). Fonts/shaping/raster: `glyphon` (cosmic-text) for now.
- Toolchain: Zig 0.16.0 (pinned in [`../mise.toml`](../mise.toml)) is required to
  build `libghostty-vt-sys`. `.cargo/config.toml` sets `LIBGHOSTTY_VT_SYS_CPU=native`.

## Crate layout (seams)

```
crates/app         (bin "ghostrealm")  winit + wgpu scene + glyphon + input routing
  src/plugin/        the plugin API: Plugin/View, PaintCx, EventCx, test harness
  src/plugins/       first-party plugins: terminal, files (+ editor, file_browser), web
crates/core        app spine: command registry, tab/split tree, inbox, sidebar order, config, fs tree, syntax highlighters
crates/lsp         language services over LSP: first-party server host + editor client
crates/process     long-lived child processes that inherit only their stdio
crates/terminal    the seam: TerminalBackend trait + Grid/Cell/Cursor/KeyPress
crates/terminal-ghostty   libghostty-vt + PTY impl of the seam
crates/web         web surface contract: JSON-line protocol, shared-memory frames, host client
crates/web-host    (bin "ghostrealm-web-host") CEF off-screen browser process; not a default member
```

- **Web surfaces run out of process.** `ghostrealm-web-host` hosts Chromium (CEF,
  off-screen rendering) and talks to the app over its stdin/stdout (one JSON object
  per line, `crates/web/src/protocol.rs`). Page pixels go through a memory-mapped,
  double-buffered BGRA file per page (`frame.rs`); the `web` plugin shows the
  newest frame with `PaintCx::image`, a texture in the same wgpu scene. Input (mouse, keys, edit
  commands) is forwarded as requests. One host serves every web surface. It is
  spawned on first use and asked to exit once no web surface has been left for
  30 s; the next web surface spawns another. `cargo build` builds and bundles it
  from the app's build script in `target/web-host` and places the bundle beside the
  `ghostrealm` binary (needs CMake + Ninja, pinned in `mise.toml`; CEF's binary
  distribution is downloaded by `cef-dll-sys`); `GHOSTREALM_NO_WEB=1` skips it. The
  app finds it beside its own binary, at `[web] host`, or `$GHOSTREALM_WEB_HOST`.

- **App ↔ terminal seam:** `TerminalBackend` (in `crates/terminal`) exposes only
  backend-neutral types. Inside `terminal-ghostty`, the PTY / VT / (future) font
  pieces are each isolated so any one can be swapped as upstream Ghostty matures.
- **Rendering is the app's, not the backend's.** The backend yields a `Grid` of
  cells; the app draws it. So terminals, chrome, and the palette share one wgpu
  scene (unified compositing; no native subviews), which also keeps it
  cross-platform.
- **App ↔ content seam:** every surface's content is a plugin `View` (terminal,
  Files surface, web page). The app owns layout, focus, chrome, and input routing;
  views paint immediate-mode through `PaintCx` and take input through `EventCx`.
  Contract: [PLUGINS.md](PLUGINS.md).

## Rendering model (current)

Per frame: every view is pumped (a terminal checks its VT worker's published
grid); then each pane's active view paints into a `Frame`'s base layer: quads
under text, images, text items, quads over text. Overlay layers stack above
it, each drawn whole (quads, images, then text through a text renderer of its
own) before the next: views' popups, then the picker, the palette, a
confirmation, the head of the toast queue, a menu, and the tooltip, each that
is open. So a confirmation covers the picker that raised it, the toast shows
(and its clock runs) over the palette, picker and confirmation, and a menu
and the tooltip cover the toast. The app draws the quads via an instanced
quad pipeline (rounded corners and borders by an SDF in the
shader), images via a textured-quad pipeline (a texture per view image,
re-uploaded only when its version changes and freed once undrawn), and the text
via glyphon. Text comes from one of three places:

- **Rows** (`RowCache`): one monospace line, keyed by content and shaped
  within a per-frame time budget (terminal grid, editor text, lists). A row
  may carry run styles (bold, italic, underline, code box) and a scale: a
  scaled row (an Integrated heading) is shaped once at the frame's metrics and
  rasterised larger (glyphon's `TextArea.scale`). An Integrated heading also
  has space above its first row (none on the view's top line); both come
  from `[headings]`.
- **Blocks** (`BlockCache`): word-wrapped proportional text with per-span
  font, size, colour, and tags (a Preview paragraph), keyed by content, width,
  and metrics, under the same budget; an unshaped block lays out at an
  estimated height.
- **Scratch** buffers for one-off labels, reshaped each frame.

A terminal snapshots only when its grid changed and resizes its PTY only when
its cell grid changes.

Files are shown by the **Files surface** (plugin `files`): a file sidebar, a
file tab per document, and each document under its document header. A text
document is an `EditorView` in one of its language's view modes: Source and
Integrated draw the buffer as rows (Integrated with styles, scaled and
spaced heading rows, and code boxes) through a per-paint `RowTable` that maps
screen y to text; Preview (Markdown) draws parsed blocks. Images and audio are a
`MediaView`. Contract and details: [PLUGINS.md](PLUGINS.md).

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
      on Enter), auto-read dwell + unfocus grace; the sidebar orders by the latest
      request for attention (`core::sidebar`). To do: OSC 133 exit code for
      unread success/failure.
- [x] Phase 6: settings TOML (load/create, wired: font, sidebar width, chrome
      colours, autohide, config-driven keybindings, sidebar `side`, chrome
      inheritance from the Ghostty config), checked against an option catalogue,
      with diagnostics and hover docs in the editor from the config language
      server. Follow-ups: completion, resolve Ghostty themes for inheritance
      (TODO.md).
- [x] Files surface (GitHub issue #1): file sidebar with name filter and `?`
      search, file tabs with a preview tab, document header with configurable
      icons, view modes with a Markdown Preview, and a table of contents.
      Follow-ups: TODO.md.

Remaining polish is tracked in [TODO.md](TODO.md). The core loop, splits, tabs,
sidebar/inbox, palette, config, and the agent channel are all in and tested.

## Verifying the GUI

The dev sandbox has no display, so `resumed` never fires there — the window,
config-file creation, and interactive paths only run on a real desktop session.
Automated coverage stands in: the workspace test suite (core logic, backend
PTY/VT, agent protocol, inbox, config, and every plugin view painted and driven
through the headless `plugin::testing::Harness`) plus an offscreen render test. Eyeball the
window by running `ghostrealm` on a real session.
