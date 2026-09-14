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
crates/app         (bin "ghostrealm")  winit + wgpu scene + glyphon + input/resize
crates/core        app spine: command registry, tab/split tree, inbox, config   [stub]
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

## Rendering model (current)

Per frame: `pump()` drains PTY output into the VT engine; `snapshot()` builds a
`Grid`. The app draws non-default cell backgrounds and the cursor via an instanced
quad pipeline, then foreground text via glyphon (one rich-text buffer per row,
coloured per cell run). Window resize recomputes cols/rows from the measured
monospace cell size and reflows the PTY + VT.

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
- [~] Phase 2: command registry [x] + tab/split tree [x] (both headless-tested);
      palette overlay + AppState integration into the app still to do.
- [ ] Phase 3: agent control/introspection channel (bumped ahead of shell polish).
- [ ] Phase 4: sidebar, splits, per-split tab strips, focus routing.
- [ ] Phase 5: status/inbox state machine.
- [ ] Phase 6: settings TOML + styling inheritance.

Next keystone: wire `core` (`Tree` + `Registry<AppState>` + a `SurfaceId ->
GhosttyTerminal` map) into the app so the window becomes a real multiplexer; that
unblocks the palette, sidebar, and agent channel. See [TODO.md](TODO.md).
