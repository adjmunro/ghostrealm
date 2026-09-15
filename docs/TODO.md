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
- Detail: A minimal editor surface now exists (`app::editor::EditorBuffer`): a
  surface is a terminal OR an editor; `editor.scratch` / `editor.open <path>`
  commands, typing/editing (arrows, Home/End, backspace/delete, newline), Cmd+S
  save, rendered via the glyphon row path with a cursor bar. Follow-ups: selection
  + copy/paste inside the editor, a modified/unsaved indicator + save-as prompt for
  scratch buffers, mouse click-to-position, horizontal scroll for long lines, and
  (later) the shell-LSP idea (IDEAS.md).
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

[2026-09-15@2e8e8c7] BUG — terminal Shift/Option+arrow keys corrupt text

- Requires: NO BLOCKERS
- Detail: In a terminal, Shift+Left deletes a char, Shift+Right deletes to end of
  line, Shift+Up = end of line, Shift+Down = start of line; Option+arrows do the
  same. Clearly a wrong modified-arrow encoding (produces sequences zsh maps to
  kill/word ops). Cmd+arrows do nothing; Ctrl+arrows are OS-handled (ignore).
- Fix: correct/normalize the modified-arrow key encoding so it stops corrupting
  input. (Then layer the editing-keys feature below.)
- Reason: current behaviour destroys the input line.

---

[2026-09-15@2e8e8c7] Editing keys — text-editor-style selection & navigation

- Requires: the modified-arrow encoding fix (above); app-side keyboard selection
- Detail: In BOTH the terminal input line and the editor pane, want standard
  editing keys: Shift+arrow selects by char; Shift+Option+arrow selects by word;
  Option+arrow moves by word; Cmd+Left/Right = line start/end; Cmd+Up/Down =
  document start / end (editor) — in the single-line terminal, Cmd/plain Up==Left,
  Down==Right; Cmd+C / Cmd+X / Cmd+V = copy / cut / paste; Shift+Enter inserts a
  newline (does NOT submit to the shell and must NOT trigger the busy dot — today
  it wrongly optimistically-busies). Terminal-line selection isn't native (the
  shell owns the line), so implement app-side keyboard selection over the grid like
  the mouse selection.
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

[2026-09-15@2e8e8c7] Command palette — scroll, MRU order, pinning, show keys, filter

- Requires: NO BLOCKERS
- Detail: (1) Results overflow the box (PALETTE_MAX) and aren't scrollable — you
  can't reach commands below the fold (e.g. the split commands). Make results
  scrollable (keep the current box size — it's liked). (2) Order by most-recently-
  used. (3) Pinning: a clickable star on the right to keep favourites at the top,
  with a pin icon (far right, keep the ▸ arrows). (4) Show a command's keybinding on
  the right in a dimmer colour (discoverability — otherwise bindings are invisible).
  (5) Filter/hide agent-only commands that make no sense for a human (e.g.
  tab.needs_input).
- Reason: usability + discoverability.

---

[2026-09-15@2e8e8c7] Commands with arguments have no UI to collect them

- Requires: NO BLOCKERS
- Detail: Commands that take args (tab.rename, editor.open, a future editor save-as)
  do nothing from the palette — there is no follow-up to enter the argument, so they
  appear broken. Add an argument-input flow: after choosing such a command, prompt
  for each required arg (reuse the Run-Anything single-line input); for a path arg,
  a file picker (see the editor entry).
- Reason: rename / open / save currently look broken.

---

[2026-09-15@2e8e8c7] Context menu — rename (scoped), htab menu, close confirmation

- Requires: the arg-input flow (rename needs text input)
- Detail: (1) Rename belongs in the right-click context menu, scoped to what was
  clicked — a workspace tab vs a horizontal/terminal tab. (2) Add a right-click
  context menu for horizontal tabs too (rename, close, …). (3) "Close" should ask
  for confirmation when it would close a workspace/pane containing more than one
  pane.
- Reason: rename is a broken palette command; context actions per tab type;
  prevent accidental loss.

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

[2026-09-15@2e8e8c7] Sidebar/workspaces — scroll, wheel routing, inbox sections, +, reorder

- Requires: NO BLOCKERS
- Detail: (1) With many workspaces (~34) the sidebar can't scroll — make it
  scrollable. (2) Mouse wheel over the sidebar should scroll the workspace list, not
  the terminal under it. (3) Implement the inbox sections: an unread / needs-input
  group at the top, then the rest by order. (4) Move the "+" new-workspace button to
  the very top (above all workspaces). (5) Drag workspace tabs to reorder.
- Reason: the email-inbox model + scale.

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

[2026-09-15@2e8e8c7] Per-workspace root directory (cwd), pinnable

- Requires: NO BLOCKERS (dir picker/arg-input helps for choosing the directory)
- Detail: A context action on a workspace tab assigns it a root directory. New
  terminal tabs AND editor surfaces created in that workspace start based in that
  directory (spawn the shell with that cwd; the editor's open/save dialogs default
  there). Scoped per workspace; changeable at any time (re-pin). Needs: store a
  root dir on the workspace (tree model), thread it through `spawn_surface` (set
  `CommandBuilder::cwd`), and a way to pick a directory.
- Reason: requested; "air traffic control" for workspaces (see IDEAS.md — autogroup
  new workspaces by directory later).

---

[2026-09-15@2e8e8c7] Taller workspace tabs showing the pinned directory

- Requires: per-workspace root directory (above)
- Detail: Make the sidebar workspace rows taller and show the directory each is
  pinned to (its root dir) under/next to the name.
- Reason: visibility of a workspace's cwd; pairs with the per-workspace root dir.

---

[2026-09-15@2e8e8c7] Config hot-reload on save (prep the app for it)

- Requires: NO BLOCKERS (foundation for the "config file is the settings UI" item)
- Detail: Re-apply config when `config.toml` changes on disk (saved from our editor
  or edited externally). Prep now: (1) factor "apply config" so it can run more than
  once — re-resolve chrome, keybindings, inbox timings, tab autohide, sidebar side/
  width, default_directory, and re-measure cell metrics if the font changed. Things
  keyed off metrics (row cache, grid geometry) must invalidate on a font/scale
  change (bump `metrics_gen`, clear caches, reflow terminals). (2) Reload trigger:
  watch the file (or reload when the editor saves that path) and reload+re-apply +
  request a redraw. Parse errors should surface non-fatally and keep the old config.
- Reason: makes editing the config feel live; underpins the settings-in-editor flow.

---

[2026-09-15@2e8e8c7] Config — default directory for new workspaces (default $HOME)

- Requires: NO BLOCKERS (feeds the per-workspace root directory)
- Detail: Add a config option (e.g. `[general] default_directory` or under a new
  section) for the base directory new workspaces/terminals start in. Defaults to
  `$HOME`; the user wants to override it to e.g. their Developer directory. A
  workspace's own pinned root dir (context action) overrides this per workspace.
- Reason: start new work where the user actually works, not always $HOME.

---

[2026-09-15@2e8e8c7] Settings = the config file opened in our editor (Cmd+,)

- Requires: text-editor improvements (the editor is the settings UI); config
  hot-reload on save
- Detail: No bespoke settings UI. `Cmd+,` opens `config.toml` in the in-app editor
  (creating it from the documented default if missing) — the file IS the settings
  UI, so lean on making the editor good instead. Requirements:
  1. The config file must be fully self-documenting. EVERY option carries a comment
     with: a description, its constraints, and its default — numeric options show
     their valid range, enum options list their variants (e.g. `side = "left" |
     "right"`), list options show the allowed entries. The user should learn what's
     available from the file alone.
  2. A "backfill / doctor" action: add any keys the user's file is MISSING (plus
     their documented comments) WITHOUT overwriting the values they've already set,
     so upgrades surface new options without clobbering customisations. (Command
     and/or offered on open.)
  3. Changes take effect on save — hot-reload the config file and re-resolve the
     dependent state (chrome, keybindings, inbox timings, default_directory, …), or
     at minimum clearly document "applies on next launch".
- Reason: simpler than a bespoke UI, keeps the file as the single source of truth,
  and doubles down on the editor; self-documenting config is discoverable.
