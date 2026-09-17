# TODO.md

## Instructions

- This files is a living document (managed by YOU) of blocked tasks and future tasks that are inconvenient to do at present.
- Append to the tail of `## Elements`.
- When looking for follow-up work, you can suggest items from this list (and your recommended priority/opinion) and/or suggest doing the subtask(s) to resolve the requirements.
- Do not remove requirements just because conditions are met.
- You can remove elements when the reason for change is redundant or no longer relevant.
- Remove from `## Elements` when completed.

## Format

- Record the datetime and current SHA at time of writing as context for posterity.
  > *Explicitly formatted as `[<datetime>@<commit-sha:0..8>]` with the square brackets - the header is not an optional field.
- Each element can have multiple bullet points for each requirement, implementation detail, and reason for change.

```markdown
---

[<datetime>@<commit-sha:0..8>]

- Requires: [<blocker(s)>|NO BLOCKERS]
- Detail: <what-to-change>
- Reason: <why-do-this-change>
```

---

## Elements

---

[2026-09-15@1d4d22e]

- Requires: NO BLOCKERS
- Detail: Remainder of Phase 5. DONE: busy detection (optimistic-on-Enter +
  foreground process-group) and the auto-read dwell timer (`inbox.auto_read_after`
  with the `auto_unread_before` unfocus grace, driven by `AppState::tick_inbox`
  and a scheduled event-loop wake). Still to do: exit code via OSC 133 to set
  `Unread { success }` truthfully (today success is always true) — needs the VT
  engine to surface OSC 133 command state.
- Reason: makes background job success/failure real (red vs green unread dot).

---

[2026-09-15@<uncommitted>]

- Requires: NO BLOCKERS
- Detail: Chrome inheritance now parses the user's Ghostty config directly
  (`core::ghostty`) for background/foreground/cursor/selection/palette when
  `[chrome]` is unset. Remaining: resolve Ghostty *themes* and `config-file`
  includes — a `theme = ...` config with no explicit `background` inherits nothing
  today (falls back to built-in defaults). Would need to locate and parse Ghostty's
  theme files.
- Reason: fuller fidelity to the brief's "resolved Ghostty palette".

---

[2026-09-15@1d4d22e]

- Requires: NO BLOCKERS
- Detail: DONE: drag to select terminal text (highlight), copy on release and via
  Cmd+C (arboard clipboard); mouse-wheel scrollback; the vtab right-click context
  menu. Still to do: OSC mouse reporting — forward mouse events to TUI apps (vim,
  htop, tmux) that enable mouse mode; needs the VT engine's mouse-mode state and
  SGR mouse encoding. (Click-to-position is skipped — not meaningful for a shell.)
- Reason: expected terminal UX; TUI apps want mouse events.

---

[2026-09-15@<uncommitted>] Verify the perf work on a real display

- Requires: a desktop session (the dev sandbox has no display; `resumed` never
  fires, so rendering/input cannot be exercised headlessly).
- Detail: The responsiveness + switch-lag rewrites (frame-clock throttle, input
  priority, budgeted pump, wake coalescing, content-keyed row cache, snapshot
  reuse) are covered by unit/integration tests but not eyeballed. Run `ghostrealm`
  and confirm: heavy `eza --tree --level=3` no longer pins the UI (tab/pane
  switches stay responsive mid-flood); clicking a vtab/split/htab is instant;
  scrolling a full screen doesn't stutter. Then delete this item.
- Reason: tests prove the mechanisms; a human must confirm the feel.

---

[2026-09-15@<uncommitted>] Double-tap triggers: make them configurable

- Requires: NO BLOCKERS
- Detail: Double-tap Shift toggles the palette and double-tap Ctrl opens Run
  Anything (`tap::TapDetector`, keeping Cmd+K). The window is now configurable
  (`[input] double_tap_window_ms`). Remaining: make the *trigger* remappable —
  which modifier maps to which action — which the chord model can't express;
  Run Anything would first need to be expressible as a command id.
- Reason: user's preferred muscle memory; keep it tunable/shareable.

---

[2026-09-15@1641f11] Sidebar context menu + new-tab button; mark-unread

- Requires: NO BLOCKERS (a popup-menu widget now exists in window.rs: `Menu`)
- Detail: DONE: right-click a sidebar vtab opens a context menu with the
  state-appropriate mark read/unread, Dismiss (when needs_input), and Close; a "+"
  new-tab button sits below the last vtab. Remaining: Rename (needs a single-line
  input overlay bound to the target vtab — reuse the palette input in a rename
  mode) and Pin (the tree has no pin/ordering concept yet).
- Reason: brief's per-vtab context actions; discoverability.

---

[2026-09-15@<uncommitted>] Text-editor pane — follow-ups

- Requires: NO BLOCKERS
- Detail: A minimal editor surface exists (`app::editor::EditorBuffer`): terminal OR
  editor; editor.scratch / editor.open <path>; typing/editing; Cmd+S save; selection
  (Shift+Arrow/Home/End) + copy/cut/paste (Cmd+C/X/V) + line/doc nav (Cmd+arrows),
  rendered with a highlight + cursor bar. Follow-ups: a modified/unsaved indicator +
  save-as prompt for scratch buffers; mouse click-to-position + drag-select in the
  editor; word motion (Option+Arrow) in the editor; horizontal scroll for long
  lines; and (later) the shell-LSP idea (IDEAS.md).
- Reason: requested; expands the app beyond terminals.

---

> The entries below are from a live testing session (2026-09-15, @2e8e8c7). They
> capture observed bugs/regressions and requests; diagnoses are hypotheses written
> while the author still had the code context.

---

[2026-09-15@2e8e8c7] Scroll lag — pacing landed; deeper cost is the render itself (P1 follow-up)

- Requires: the worker-thread architecture (below) for the full fix
- Detail: DONE (b173b08): wheel events now apply the scroll to the VT immediately but
  pace the redraw through the frame clock (frame_pending) instead of an immediate
  request_redraw per event, so a flick coalesces into ~one render/frame instead of a
  vsync-blocked backlog. REMAINING: each paced frame still re-snapshots the grid and
  reshapes newly-revealed rows on the UI thread (~fine in release, heavier in debug);
  the worker-thread architecture removes that from the UI thread entirely. Verify the
  feel after that lands; if still stuttery, profile the snapshot/reshape.
- Reason: user's #1 priority — scroll must feel instant.

---

[2026-09-15@2e8e8c7] Responsiveness — heavy output (`eza --tree`) beachball — likely fixed by threading

- Requires: verify on a real display
- Detail: The beachball came from VT parse + snapshot running on the UI thread. The
  worker-thread architecture (92d5ca0) moves all of that off the UI thread, so
  switching workspace/pane during a flood should stay responsive now. Only reshape
  (glyphon) remains on the UI thread, bounded by the content-keyed row cache.
  VERIFY, then close. If a flood still causes reshape-bound jank, cap reshape work
  per frame (render partial, continue next frame).
- Reason: a multiplexer must stay responsive under load.

---

[2026-09-15@2e8e8c7] Editing keys — bracketed paste + editor word motion remain

- Requires: NO BLOCKERS
- Detail: DONE — terminal (785bf9a): Shift+Arrow grid selection (word with Option),
  Option+Left/Right word motion (ESC-b/f), Cmd+Left/Right/Up/Down line motion, Cmd+C
  copy, Cmd+V paste, Shift+Enter sends LF without a false busy dot (262af58). Editor
  (f591f3d): Shift selection, Cmd+C/X/V, Cmd+arrow line/doc nav. REMAINING: (1)
  bracketed-paste wrapping so a multi-line terminal paste doesn't auto-execute each
  line; (2) Option+Arrow word motion IN the editor (currently char move).
- Reason: expected editor behaviour; the muscle memory the user relies on.

---

[2026-09-15@2e8e8c7] Naming — rename "vertical tabs" → "workspaces"

- Requires: NO BLOCKERS
- Detail: In the UI, call vertical tabs "workspaces". Command labels are confusing:
  "New Tab" / "Close Tab" / "Rename Tab" act on vertical tabs, not the pane's
  horizontal tabs. Rename to New Workspace / Close Workspace / Rename Workspace, and
  the pane htab actions to "New/Close Terminal Tab". Consider a scope prefix in
  palette labels (e.g. leading `[workspace]` / `[terminal tab]`). Update GLOSSARY.md.
  Internally, rename vtab → "workspace tab" where reasonable.
- Reason: reduce confusion; consistent vocabulary (we already call it a workspace).

---

[2026-09-15@2e8e8c7] Palette — a "hide" button per command (config-persisted)

- Requires: config write-back (persist the hide list); builds on CommandMeta::hidden
- Detail: (user idea, IDEAS.md) A clickable hide control on a palette row appends the
  command id to a hide list in the config; hidden ids are filtered out thereafter
  (undoable by editing config). Ship a default "hide from human" list for commands
  that aren't useful via search (things with real UI buttons / shortcuts) — today
  only `hidden()` is hardcoded in the registry. Maybe also a "hide from agent" list
  restricting which commands are usable over MCP.
- Reason: keep the palette focused; user-tunable.

---

[2026-09-15@2e8e8c7] Command palette — MRU order + pinning (rest done)

- Requires: persistent per-user state (MRU history, pins)
- Detail: DONE (63124fc): scrollable results (10-row window over up to 100 ranked
  matches), keybindings shown right-aligned in a dim colour, and agent-only commands
  (CommandMeta::hidden()) excluded from the human palette. REMAINING: (1) order by
  most-recently-used (needs a persisted usage history); (2) pinning — a clickable
  star on the right to keep favourites at the top (needs persisted pins). Both want
  a small persistent store (or a config section).
- Reason: usability + discoverability.

---

[2026-09-15@2e8e8c7] Commands with arguments — arg prompt done; file picker remains

- Requires: NO BLOCKERS
- Detail: DONE (ace4f7c): choosing a command with required args (Rename Workspace,
  Open File in Editor) now prompts for each arg in the palette (name + description +
  kind, enum options listed), validated per kind, then runs it. REMAINING: for a
  path arg, a proper file picker instead of typing the path (ties to the editor
  save-as/open dialog entry).
- Reason: rename / open now work; path entry is still raw typing.

---

[2026-09-15@2e8e8c7] Context menu — htab menu + close confirmation (workspace rename/set-dir done)

- Requires: NO BLOCKERS
- Detail: DONE (a3a8ef1): the workspace right-click menu now has Rename… and Set
  directory… (they open the argument prompt). REMAINING: (1) a right-click context
  menu for horizontal/terminal tabs (rename that surface, close it, …) — needs a
  per-surface rename command; (2) "Close workspace" should confirm when the workspace
  has more than one pane (avoid accidental loss) — needs a small confirm step (reuse
  the Menu widget with Confirm/Cancel, or a dedicated confirm overlay).
- Reason: context actions per tab type; prevent accidental loss.

---

[2026-09-15@2e8e8c7] Splits — discoverability, draggable resize handles, ratios

- Requires: NO BLOCKERS
- Detail: (1) Split commands are hard to find (buried below the palette fold) — add
  default keybindings and surface them. (2) Draggable split handles to resize panes
  with the mouse. (3) A split currently halves the *current* pane; want free
  resizing (including small panes) rather than a fixed half.
- Reason: expected tiling UX.

---

[2026-09-15@2e8e8c7] Horizontal tabs — overflow scroll, drag-reorder, drag-to-split

- Requires: NO BLOCKERS
- Detail: (1) htabs shrink as you add more; add horizontal scroll when they overflow.
  (2) Drag a htab (its name area, shown only when >1 tab) to reorder. (3) Drag a htab
  out to a pane edge (side/top/bottom) to break it into a split — the drag-to-split
  idea in IDEAS.md.
- Reason: expected tab UX; requested drag-to-split.

---

[2026-09-15@2e8e8c7] Sidebar/workspaces — inbox sections + drag-reorder (scroll/wheel/+ done)

- Requires: NO BLOCKERS
- Detail: DONE (2d7c341): scrollable workspace list, wheel-over-sidebar scrolls it,
  and the "+" button pinned at the very top. REMAINING: (3) inbox sections — group
  unread / needs-input at the top, then the rest by order; (5) drag workspace rows
  to reorder (the tree stores workspaces in an ordered Vec — reordering needs a move
  op + drag handling in the sidebar).
- Reason: the email-inbox model + reordering.

---

[2026-09-15@2e8e8c7] Editor — save-as / open dialogs (file picker)

- Requires: the arg-input flow
- Detail: `editor.scratch` has no path, so Cmd+S can't save (no save-as dialog);
  `editor.open` needs a path with no picker, so it does nothing. Add a save-as
  dialog and a file picker — prefer a custom in-theme dialog (native acceptable as a
  fallback). Ties into the arg-collection entry. Also the editor follow-ups already
  listed (in-editor selection/copy/cut/paste, modified indicator, click-to-position,
  word-nav).
- Reason: editor save/open are currently unusable.

---

[2026-09-15@2e8e8c7] ARCHITECTURE — VT off the UI thread — DONE (92d5ca0)

- DONE: `ThreadedTerminal` runs each terminal's VT+PTY on a worker thread; the UI
  only renders + sends input messages and reads published `Grid` snapshots. No VT
  parsing or snapshotting on the UI thread. AppState.surfaces hold ThreadedTerminal.
- Verify on a real display: heavy `eza --tree` should no longer beachball, and
  workspace/pane switching should stay responsive mid-flood.
- Follow-ups (optional): rate-limit the worker's grid builds under a sustained flood
  (it currently builds per drained batch — fine, off the UI thread, but wasteful);
  recycle the UI's consumed grids back to the worker; kill the child on drop.

---

[2026-09-15@2e8e8c7] Per-workspace root directory — core done; picker + context menu remain

- Requires: NO BLOCKERS
- Detail: DONE (1eccbec): Vtab.root_dir; new terminals spawn with the resolved cwd
  (workspace root -> config default_directory -> shell default); `workspace.set_root`
  command (path arg) pins the active workspace's root. REMAINING: (1) a context-menu
  action to set it (currently only via the palette command's typed path); (2) a
  directory picker instead of typing; (3) editor surfaces + the editor's open/save
  dialogs should default to the workspace root too.
- Reason: requested; "air traffic control" for workspaces (see IDEAS.md — autogroup
  new workspaces by directory later).

---

[2026-09-15@2e8e8c7] Taller workspace tabs showing the pinned directory

- Requires: per-workspace root directory (above)
- Detail: Make the sidebar workspace rows taller and show the directory each is
  pinned to (its root dir) under/next to the name.
- Reason: visibility of a workspace's cwd; pairs with the per-workspace root dir.

---

[2026-09-15@2e8e8c7] Settings = the config file in our editor — core done; self-doc + backfill remain

- Requires: NO BLOCKERS
- Detail: DONE (64a72b0): Cmd+, opens config.toml in an editor pane (created with
  defaults if absent); saving it (Cmd+S) hot-reloads — apply_config re-resolves
  chrome/inbox/default_directory and re-measures metrics on a font change (invalidate
  shaping cache + reflow), keybindings/sidebar read live. REMAINING:
  1. Make the config fully self-documenting: EVERY option's comment gives a
     description, its constraints (numeric range, enum variants like
     `side = "left" | "right"`, list options), and its default. Today only some
     sections are documented.
  2. A "backfill / doctor" action: merge MISSING keys (+ documented comments) into
     an existing config WITHOUT overwriting the user's set values, so upgrades
     surface new options without clobbering customisations.
  3. Hot-reload on EXTERNAL edits (a file watcher) — today only our editor's save
     triggers a reload. Also surface parse errors non-fatally (keep the old config).
- Reason: the file is the settings UI; keep it discoverable and upgrade-safe.

---

[2026-09-16@8eae419]

- Requires: NO BLOCKERS (but assess necessity first — see Reason)
- Detail: Scroll-perf idea #4 — move terminal-row shaping AND glyphon `prepare`
  off the UI thread onto a dedicated render thread, double-buffering the prepared
  frame (build frame N+1's shaped rows + vertices while the GPU draws frame N,
  then swap). Both `shape_until_scroll` and `TextRenderer::prepare` need
  `&mut FontSystem`, so they must co-locate on one thread — don't split them, and
  don't try per-thread FontSystems (a Buffer's cache keys are bound to the
  FontSystem that shaped it). Real cost is that ALL text prep (sidebar, palette,
  strip, editor, terminal) must migrate to that thread since FontSystem can live
  in only one place, plus the swap/sync plumbing (double/triple-buffered vertex +
  atlas buffers, fences). Keep swapchain acquire+present on the UI thread. This is
  the convergence point for the other deferred perf ideas too: off-thread/parallel
  shaping and a content-keyed, position-independent per-row GEOMETRY cache (idea
  #8, which would extend partial-rerender wins to the editor, partial TUI updates,
  AND scrolling). Treat #4 + #8 + parallel/custom-monospace-layout as one
  "proper renderer" epic, not separate tasks.
- Reason: Highest ceiling (UI frame never blocks on text work, complete frames
  with no fill-in), but the biggest, riskiest change. FIRST re-measure after the
  landed cheap wins (trailing-blank trim, ASCII Basic shaping, atlas pre-warm,
  per-frame shaping budget — b474a25, 83ef4c5, 7dd6973, 8eae419): if scrolling is
  already buttery and idle/typing/TUI/editor frames are cheap enough, #4 becomes
  optional polish rather than a needed fix. Only invest if profiling shows
  UI-thread shaping/prepare is still the bottleneck. Related IDEAS.md note: "Do
  we re-render the whole screen when dirty? Or can we render only part of it?"

---

[2026-09-17@584a284]

- Requires: NO BLOCKERS
- Detail: Harden config loading against malformed/unexpected input. Today a parse
  error falls back to Config::default() (non-fatal, good), but audit for: values
  out of documented range (clamp rather than accept/crash), enum/string fields
  with bad variants, negative/zero sizes, huge numbers, non-UTF8, partial tables,
  and the authoritative [keybindings] block (bad chords, unknown command ids,
  duplicate chords). Prefer clamp-and-warn over reject-whole-file where sensible.
- Reason: the config is user-edited (and hot-reloaded); a bad edit should never
  crash the app or silently wipe settings.

---

[2026-09-18@584a284]

- Requires: NO BLOCKERS
- Detail: Config colours should accept flexible inputs — hex ("#rrggbb",
  "#rrggbbaa", short "#rgb") and rgb()/rgba() — and carry an alpha channel, so the
  window/panes can be made semi-translucent. Needs: a colour parser + serde
  for the config colour fields, threading alpha through Chrome/rect_quad (already
  takes an alpha arg) and the wgpu surface (transparent framebuffer + composited
  window; on macOS set the window/layer opaque=false and clear with alpha).
- Reason: nicer theming and a translucent-terminal aesthetic.

---

[2026-09-18@6577576]

- Requires: NO BLOCKERS
- Detail: Soft-wrap follow-ups. Editor soft-wrap + per-pane ribbon toggle are DONE
  (d04ef0d): greedy word-wrap, gutter/cursor/selection/click all wrap-aware,
  `[editor] soft_wrap` default on. REMAINING: (1) the toggle only shows on
  single-tab editors (in the ribbon); add it for multi-tab editors and, if wanted,
  terminals (terminals already wrap to their column count, so a terminal toggle
  would need a ghostty reflow/no-wrap mode — likely skip). (2) Wrapped editors
  re-shape visible rows via the row cache but char-substring keys mean many small
  entries; fine for now, revisit if a huge wrapped file feels slow.
- Reason: complete the toggle's reach; keep perf healthy.

---

[2026-09-18@09c015c]

- Requires: NO BLOCKERS
- Detail: Editor undo/redo (Cmd+Z / Cmd+Shift+Z). Needs an edit-history stack on
  EditorBuffer (snapshots or a reversible op log with coalescing of consecutive
  typing into one undo step), wired to the key handler. User has further ideas on
  scope/behaviour — discuss before building.
- Reason: basic editing expectation; currently edits can't be undone.
