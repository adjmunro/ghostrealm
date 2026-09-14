# Project brief: a terminal multiplexer built on libghostty (Rust)

Paste this whole file as your first message to an agent on the target machine.

---

## What we're building

A native desktop **terminal multiplexer** that hosts **libghostty** terminal surfaces inside our own window chrome. libghostty is a black box — we never touch VT parsing, PTY, rendering, or fonts. We own everything *around* it: a window, a **vertical tab sidebar**, **splitting (both directions) with per-split horizontal tabs**, a **command palette**, and an **agent-facing control channel (MCP)**.

Target platforms: **macOS and Linux** (Windows is out — libghostty doesn't support it; revisit only if that changes). Ship macOS first.

## Locked stack decisions

- **Language:** Rust (single codebase, C-ABI FFI to libghostty via `bindgen`).
- **Windowing / event loop:** `winit`.
- **GPU / our own drawing:** `wgpu`.
- **Config format:** TOML dotfile (XDG path, e.g. `~/.config/<app>/config.toml`).
- **Agent control:** MCP server hosted in-app, thin generic tool set (see below). Toggle on/off in settings. Set default in config.

## Do this FIRST — the load-bearing unknown

Before writing app code, **fetch the current `ghostty.h` (and any Rust example/crate) and confirm the embedding C API**. The embed API is pre-stable; the Swift macOS app is the reference embedder. Verify it exposes, and write a short findings note on:

1. **Surface lifecycle** — create app, create surface into a native view/layer we supply, resize, focus, destroy.
2. **Native handle attach** — what it expects to draw into (`CAMetalLayer`/`NSView` on macOS; X11/Wayland on Linux). Confirm we can hand it a `raw-window-handle` surface positioned at an arbitrary rect.
3. **Child process state** — can we observe running vs exited, and exit code? (drives tab status)
4. **Title callback** — OSC 0/2 title changes surfaced to the embedder? (free tab labels)
5. **Semantic/progress events** — does it forward OSC 133 (command start/end + exit) and OSC 9;4 (progress)? (drives status without cooperation)
6. **Config** — can we point a surface at a Ghostty config file / read its resolved palette & opacity? (chrome styling inheritance)

If any of 3–6 aren't exposed, note the fallback (e.g. foreground-process-group polling on the PTY fd for busy/idle; rely on the MCP back-channel for `needs_input`). **Report findings before building** — they decide how much is free vs DIY.

## Architecture

- **Core (platform-agnostic Rust):** the command registry, tab/split tree model, tab status/inbox state machine, config load/save, keybinding resolution. This is the spine; build it first and test it headless.
- **Shell (thin per-OS seam):** window + event loop (`winit`), our chrome rendering (`wgpu`), and the libghostty surface-attach shim (mostly shared, small `#[cfg(target_os)]` branches for the native handle). The terminal panes are native surfaces composited over our drawn chrome.
- **Data flow:** the core is the brain. Events flow *in* (keys, clicks, surface callbacks, agent calls); the core updates state and emits intents *out* (split, relayout, focus, spawn). The shell owns the runloop.

## The command registry (build this early — it's the spine)

Every action the app can perform is a named command: `{ id, title, description, args schema, handler }`. Three consumers off one list:

1. **Command palette** — a JetBrains "Search Everywhere"-style popup; fuzzy search over the registry. Render it as a **floating panel above everything** (a separate top-level layer/window), NOT painted into the terminal's view tree — native GPU surfaces resist compositing, so overlays must sit above them.
2. **Keybindings** — optional sugar in settings that invoke a command id. Bare minimum sensible defaults only; everything else is discoverable via the palette or buttons we add to the UI.
3. **Agents (MCP)** — the same registry, exposed as tools.

## MCP design (thin, context-frugal)

Do **not** emit one MCP tool per command — that spams the model's context. Expose the registry through a small fixed tool set:

- `search(query)` → returns only matching command ids + arg schemas. Maybe also have a `list` or `help`?
- `set(id, args)` → executes. Some ids will enable a program/agent running *inside* a tab to report its own state, or change things about the app, e.g. set its tab label, resize etc.
- `get(id, args)` → current tabs/splits/statuses, names, sizes etc, if useful. Provide text-based equivalent/measures of graphical things so that the LLM can get a read without a screenshot.

An agent should be able to read the current state, make a decision, and mutate the app via `set`. E.g. i should be able to ask an LLM to fix "x" without knowing how to do it or where in settings it is. Also, the LLM should have full access so that it can automatically test the app and verify the behaviour. Though it should be scoped/sandboxed to only the vertical tab it's in (and also app-wide settings), unable to affect other vertical tab canvases.

This closes a nice loop: an agent (e.g. Claude) running in a tab both **drives** the app (search/run) and **reports** its own status/label back to it.

## Feature spec

### Vertical tab sidebar (the "inbox" — email model)
Tabs behave like an email inbox: **read / unread**, plus a sticky **needs_input**.

- Left by default; `sidebar.side = "left" | "right"` in settings.
- **States:**
  - `read` — nothing new to see.
  - `busy` — a command/foreground child is running.
  - `unread` — a command **completed while the tab was unfocused** (carries exit success/failure). If a command completes while you are *looking at* the tab, it stays `read` — you saw it (remark as unread if you clicked away to another tab or another app/window in less than 10(?) seconds). This is what prevents unread-spam for interactive terminal use while still flagging a background job that finished.
  - `needs_input` — running but blocked awaiting the user. **Sticky.**
- **Layout — two sections:**
  - **Inbox (top):** all `unread` and `needs_input` tabs, newest activity first (a fresh `unread` can sit above an older `needs_input`).
  - **Read (below):** everything else, in the user's manual order.
- **Clearing:**
  - `unread` → `read` after **continuous focused dwell ≥ `autoread_after`** (default `60s`(?) — LLM output takes minutes, so a glance isn't enough), or manual **Mark read** from context menu on vertical tab (which just runs the related command id/args from the registry). Dwell resets on unfocus (e.g. looking at another app or screen or tab).
  - `needs_input` → clears **only** when the block resolves (input given / program advances) or manual **Dismiss**. **Never** cleared by viewing.
  - On clearing, a tab leaves Inbox and drops into Read. `needs_input` persists in Inbox until resolved, even as newer unreads rise above it and then drop away.
- **Per-tab context actions** (right-click a vertical tab): **Mark read** / **Mark unread**, **Dismiss** (when `needs_input`), pin/rename, close. These are all just UI buttons backed by the command registry.
- **Status sources**, best-effort layered: OSC 133 (command completion + exit code) → title/OSC 9;4 (progress) → foreground-process-group polling (busy vs read) → MCP back-channel (`needs_input`, and the richest signal). `needs_input` is hardest to auto-detect; primary path is self-report.
- **Design note:** the manual mark read/unread actions make the auto-read timing low-stakes — pick the sane default and don't over-tune.
- **Tab labels:** default to the program-set title (OSC 0/2). If the user hasn't pinned a name, an agent may rename via command registry actions / mcp. A user-set name is sticky and wins.

```toml
[inbox]
auto-read-after = "60"   # continuous focus before unread -> read; "0" = manual only
auto-unread-before = "10" # reset if went unfocused x seconds after busy->read
```

### Splitting & per-split tabs
- The remaining area (right of sidebar) supports **splits in both directions**, recursively (binary split tree; leaves = split panels).
- Each split panel has its **own horizontal tab strip**.
- `autohide_single_tab = true` (setting): hide a split panel's tab header when it holds only one tab.

### Settings — TOML dotfile
- Single shareable TOML at an XDG path. Sections: `[sidebar]`, `[inbox]`, `[keybindings]`, `[tabs]`, `[chrome]`, `[terminal]`.
- Designed to be committed/synced as a dotfile.
- Auto generate / populate with defaults and one-line comments descriptions.

### Styling
- **Terminal styling** (colours, transparency/opacity, font) **inherits from Ghostty's own config file per terminal** — read/point libghostty at it rather than reinventing.
- **Chrome styling** (sidebar, tab strips, palette): optional `[chrome]` overrides; when unspecified, **inherit from the resolved Ghostty palette/opacity** so the app looks coherent by default.

## Open decision to make early

- **Chrome UI framework.** Options over `wgpu`: hand-rolled renderer, `egui` (fastest to iterate, immediate-mode look), `iced` (retained/Elm), or GPUI (Zed's — powerful, terminal-grade, less documented). Recommendation: prototype the palette + sidebar in `egui` to move fast, keep the rendering behind a trait so it can be swapped. Decide before Phase 2.

## Suggested phases

1. **Verify** `ghostty.h`; write findings note. Spike: one libghostty surface in a `winit`+`wgpu` window, resizing correctly. This de-risks everything.
2. **Core**: command registry + palette (floating panel) + tab/split tree model, tested headless.
3. **Shell**: vertical sidebar, splits, per-split tab strips, focus routing, `autohide_single_tab`.
4. **Status/inbox**: wire the status sources; implement inbox hoisting + acknowledgement.
5. **MCP**: thin tool set; the report-status back-channel; agent-editable labels.
6. **Settings + styling**: TOML load/save; Ghostty config inheritance; chrome overrides.

## Working agreements

- **Verify before you build.** Confirm the API exposes something before designing on it; state assumptions explicitly.
- **Small, atomic commits**; separate behavioural changes from moves/renames; conventional-commit headers with a why-focused body.
- Keep the native seam **small and isolated** — most day-to-day code should be safe Rust; quarantine `unsafe`/objc to the surface-attach shim.
- Report outcomes faithfully (failing tests, skipped steps) rather than glossing.
- Unit test failures include next steps for LLM so that it knows how to take corrective action.
