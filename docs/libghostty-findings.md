# libghostty embedding — findings

Phase 1 verification of the embedding C API, done against `ghostty-org/ghostty@main`
(`include/ghostty.h`, `include/ghostty/vt.h`, `build.zig`). Read before designing on
the embed API.

## TL;DR — there is no single library matching the brief's assumption

The brief assumes one black box that owns VT parsing + PTY + rendering + fonts and
accepts a native surface at an arbitrary rect on **both macOS and Linux**. No such
library exists. There are two distinct products, and you must pick:

| | **libghostty-internal** (`ghostty.h`) | **libghostty-vt** (`ghostty/vt.h`) |
|---|---|---|
| What it is | Full embedder: surface = PTY + Metal renderer + fonts | VT engine only: parser + terminal state + input encoding |
| "Black box" surface? | **Yes** — hand it a view, it runs a terminal | **No** — you build PTY, renderer, fonts around it |
| Platforms (for surfaces) | **macOS + iOS only** | Cross-platform (macOS, Linux, even WASM) |
| Native handle it wants | `NSView*` (macOS) / `UIView*` (iOS) — **no Linux** | n/a (no surface concept) |
| Stability / docs | Pre-stable, **undocumented**, "not designed for external use" (its own header) | Documented, ABI type-manifest validated |
| Rust bindings | None published — we write `bindgen` FFI ourselves | Exist and are good: `libghostty-vt` / `libghostty-vt-sys` crates |
| Build | Build Ghostty from Zig source, `app-runtime=none` → `ghostty-internal.{so,dll}` | `zig build` shared/static lib, or use the crates |

`build.zig` is explicit that the internal lib "is NOT libghostty (even though it's
named that for historical reasons)". Upstream steers external embedders to
libghostty-vt. A comparable product (Paneflow) ships on Linux using **vt + its own
renderer**, not the internal embedder.

**Consequence:** the brief's "we never touch VT parsing, PTY, rendering, or fonts"
holds only on macOS via the internal embedder. Cross-platform means owning the
renderer + PTY + fonts on top of the vt parser. This is the load-bearing decision;
see Options below.

## Answers to the six verification questions (internal embedder)

All grounded in `ghostty.h`. Function names are exact.

1. **Surface lifecycle — YES.**
   - `ghostty_app_new(runtime_config, config)` / `ghostty_app_free`
   - `ghostty_surface_new(app, surface_config)` / `ghostty_surface_free`
   - Resize: `ghostty_surface_set_size(s, w_px, h_px)`; query `ghostty_surface_size` →
     cols/rows/px/cell px.
   - Focus: `ghostty_surface_set_focus(s, bool)`; occlusion `ghostty_surface_set_occlusion`.
   - Destroy: `ghostty_surface_request_close` / `ghostty_surface_free`.
   - Redraw driven by host: `ghostty_app_tick`, `ghostty_surface_draw`, `ghostty_surface_refresh`.

2. **Native handle attach — PARTIAL (macOS/iOS only).**
   - `ghostty_surface_config_s.platform` is a union of `{ nsview }` / `{ uiview }`;
     `ghostty_platform_e` = INVALID / MACOS / IOS. **No X11/Wayland member.**
   - macOS: we supply an `NSView*`; Ghostty attaches its own `CAMetalLayer` and renders
     with Metal into it. Arbitrary rect = we control that NSView's frame as a subview.
   - This means **terminal and chrome are sibling native layers**, not one GPU scene —
     see Compositing below.
   - Linux: no native-handle path here at all.

3. **Child process state — YES.**
   - `ghostty_surface_process_exited(s) -> bool` (running vs exited).
   - Exit code via action `GHOSTTY_ACTION_SHOW_CHILD_EXITED` →
     `ghostty_surface_message_childexited_s { uint32_t exit_code; uint64_t timetime_ms }`
     (note: field is literally misspelled `timetime_ms` in the header).
   - Also `ghostty_surface_foreground_pid(s)` and `ghostty_surface_tty_name(s)` →
     enables foreground-process-group polling for busy/idle.

4. **Title callback — YES (free tab labels).**
   - Actions `GHOSTTY_ACTION_SET_TITLE` (`{ const char* title }`), plus `SET_TAB_TITLE`,
     `SET_WINDOW_TITLE`, `PROMPT_TITLE`. OSC 0/2 arrives as `SET_TITLE`.

5. **Semantic / progress events — YES, richer than hoped.**
   - `GHOSTTY_ACTION_COMMAND_FINISHED` → `{ int16_t exit_code (-1 = none, else 0-255);
     uint64_t duration_ns }`. This is OSC 133 command-end + exit + duration — directly
     drives inbox unread + success/failure.
   - `GHOSTTY_ACTION_PROGRESS_REPORT` → `{ state; int8_t progress (-1 or 0-100) }` (OSC
     9;4). States: REMOVE / SET / ERROR / INDETERMINATE / PAUSE.
   - Bonus: `RING_BELL`, `DESKTOP_NOTIFICATION`, `PWD`, `SELECTION_CHANGED`,
     `MOUSE_OVER_LINK`, `START_SEARCH`/`END_SEARCH`/`SEARCH_TOTAL`.
   - **Gap:** no explicit *command-start* action (OSC 133 A/B). "busy" must be inferred
     from foreground_pid polling and/or progress activity, with COMMAND_FINISHED marking
     the end. Matches the brief's fallback plan. `needs_input` is not detectable here at
     all → rely on the MCP self-report back-channel (as the brief anticipated).

6. **Config / styling inheritance — YES, strong.**
   - `ghostty_config_new`, `ghostty_config_load_file(cfg, path)`,
     `..._load_default_files`, `..._load_recursive_files`, `ghostty_config_finalize`.
   - Read resolved values: `ghostty_config_get(cfg, out*, key, len)`; palette/color structs
     (`ghostty_config_palette_s { color[256] }`, `ghostty_config_color_s {r,g,b}`) → chrome
     inherits the resolved Ghostty palette/opacity.
   - New tabs/splits inherit via `ghostty_surface_inherited_config(s, context)`.
   - Live restyle: actions `GHOSTTY_ACTION_COLOR_CHANGE`, `CONFIG_CHANGE`, `RELOAD_CONFIG`.

## Event loop, threading, dispatch (internal embedder)

- **Host drives the loop.** libghostty does not own a runloop. We pump
  `ghostty_app_tick`; libghostty calls `wakeup_cb(userdata)` to ask us to wake and tick
  again → integrate with winit via an `EventLoopProxy` user-event. `GHOSTTY_ACTION_RENDER`
  requests a redraw → we call `ghostty_surface_draw`.
- **One callback for everything.** `ghostty_runtime_config_s.action_cb(app, target, action)`
  dispatches all `GHOSTTY_ACTION_*`. This includes multiplexer intents Ghostty's own
  keybindings emit — `NEW_TAB`, `NEW_SPLIT`, `GOTO_TAB`, `GOTO_SPLIT`, `RESIZE_SPLIT`,
  `MOVE_TAB`, `CLOSE_TAB`, `TOGGLE_COMMAND_PALETTE`, … **We own the tab/split tree**
  (per glossary: panels/htabs/vtabs); Ghostty surfaces are leaves. We choose to honor or
  ignore each requested action. `ghostty_surface_binding_action(s, name, len)` lets us
  invoke any Ghostty action by name; `ghostty_surface_split*` are convenience triggers.
  This validates the brief's "core owns the tree, surfaces are native leaves".
- Other callbacks: clipboard read/confirm/write, `close_surface_cb`, `wakeup_cb`. App is
  single-threaded from libghostty's perspective — keep all calls on the UI thread.

## Compositing model (macOS, internal embedder)

Ghostty renders terminals with **its own Metal layer on the NSView we give it**. Our
chrome (sidebar, tab strips) is a **separate** wgpu layer; the command palette is a
separate top-level panel/child window above everything. AppKit's view hierarchy does the
compositing — we do **not** blend Ghostty's output into our wgpu render pass. This is
exactly why the brief says overlays must be their own layer above the native surfaces.
Practical seam: get the `NSWindow`/content `NSView` from winit (`raw-window-handle`), add
each terminal surface's `NSView` as a subview at a computed rect, put the wgpu chrome
layer behind, put the palette panel on top. This is the one place `unsafe`/objc lives.

## Linux reality

The internal embedder exposes **no** Linux surface handle. Linux terminals therefore
require the **vt + own-renderer** path (Option B). There is no recompile-and-ship route
from a macOS internal-embedder build to Linux. Treat Linux as a separate backend, not a
target flag.

## Options

**A — Internal embedder, macOS-first (matches the brief's black-box vision).**
- Get a real terminal on screen in the Phase 1 spike; zero renderer/font work.
- Cost: macOS/iOS only; pre-stable undocumented API pinned to a Ghostty commit; we write
  the `bindgen` FFI ourselves; churn risk; native-subview compositing seam. Linux deferred
  to a future vt-based backend.

**B — libghostty-vt + our own wgpu renderer/PTY (the cross-platform path).**
- Reaches the real cross-platform goal; documented, ABI-managed; ready Rust crates;
  precedent (Paneflow on Linux). Unified wgpu scene → trivial overlay compositing.
- Cost: we build the GPU text renderer (glyph atlas, shaping, fonts), cursor/selection,
  and PTY spawn/reflow wiring. Weeks of work before the first glyph, all of it undifferentiated
  vs the product's actual novelty (the chrome/inbox/MCP).

**Hybrid — trait-seam now, both later.** Define the terminal-pane behind a trait (the brief
already wants rendering swappable). Ship A on macOS; add a B backend for Linux when it
matters. The compositing model differs per backend (native subview vs wgpu-drawn), so the
trait must abstract *placement + input + status*, not assume one scene.

## Recommendation

Start with **A behind a trait seam** (i.e. the hybrid, A-first). Rationale: the product's
novelty is entirely in the chrome — sidebar inbox, splits, palette, MCP — not in terminal
rendering. A puts a working terminal under that superstructure fastest on the primary
platform and keeps us out of the font-rendering swamp, while the trait keeps a vt-based
Linux backend open. The main tax to accept is tracking a pinned, undocumented internal API.

If **Linux is a launch requirement rather than "revisit later"**, that inverts the call:
go **B** from the start, because A produces nothing reusable for Linux surfaces.

## Risks / to verify in the spike

- Building `ghostty-internal` from Zig source and linking it from Rust (`bindgen` against
  `ghostty.h`); pin the commit.
- winit ↔ libghostty runloop handshake (`wakeup_cb` → `EventLoopProxy` → `ghostty_app_tick`).
- Adding a Ghostty surface `NSView` as a subview of the winit `NSWindow` at an arbitrary
  rect, resizing correctly under HiDPI (`ghostty_surface_set_content_scale`).
- Reading resolved palette/opacity via `ghostty_config_get` for chrome inheritance.

## Build toolchain — VERIFIED (Option B, 2026-09-14, macOS 26.6.2 / aarch64)

A throwaway crate fed VT bytes through `libghostty-vt` and read the cell grid back
(`Terminal::new` → `vt_write` → `RenderState` → `RowIterator`/`CellIterator` →
`cell.graphemes()`). Confirmed working. The path to that was not obvious:

- **Dependency: git, not crates.io.** `crates.io` `libghostty-vt@0.2.1` pins a Ghostty
  commit requiring Zig 0.15.2. Depend on the git repo instead, pinned to a rev:
  `libghostty-vt = { git = "https://github.com/uzaaft/libghostty-rs", rev = "5988a0b78b4aa804d1c12e66bbfe662bd97d81c0" }`
  (pins Ghostty `22d13172`, `minimum_zig_version = 0.16.0`).
- **Zig 0.16.0 exactly.** `libghostty-vt-sys/build.rs` fetches Ghostty at the pinned
  commit and builds it with Zig. Ghostty's `requireZig` enforces exact major.minor.
  Zig is pinned in [`../mise.toml`](../mise.toml).
- **macOS 26 SDK needs Zig ≥ 0.16.** On this macOS 26.6.2 host the only SDK is
  `MacOSX26.5.sdk`; Zig 0.15.2's bundled linker cannot resolve its `libSystem.tbd`
  (undefined `_printf` etc. even for a trivial `zig cc`), while 0.16.0 links it. This is
  independent of Ghostty — a pure Zig-vs-new-SDK issue.
- **`LIBGHOSTTY_VT_SYS_CPU=native`** for local builds; omit for a distributable baseline
  binary. Set it in the build environment (or a cargo config), not in source.
- First clean build ≈ 1m47s (network fetch + Zig compile of Ghostty), cached after.
- Ghostling reference (`example/ghostling_rs`) renders with **macroquad** — a wiring
  reference only; we render with wgpu.

To bump the Ghostty version later: update the binding `rev` to a newer
`uzaaft/libghostty-rs` commit, then match `mise.toml`'s Zig to that Ghostty's
`build.zig.zon` `minimum_zig_version`.
