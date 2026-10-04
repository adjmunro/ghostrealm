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
  editor; editor.new / editor.open <path>; typing/editing; Cmd+S save; selection
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

[2026-09-15@2e8e8c7] Commands with arguments — arg prompt done; generic path args remain

- Requires: NO BLOCKERS
- Detail: DONE (ace4f7c): choosing a command with required args (Rename Workspace)
  prompts for each arg in the palette (name + description + kind, enum options
  listed), validated per kind, then runs it. DONE: commands whose only arg is a
  path make it optional and open the floating picker without it
  (`workspace.new`, `editor.open`); `workspace.pick_dir` always picks.
  REMAINING: a `Path` arg kind (dir or file) the palette's arg prompt fills from
  the picker itself, so a command needn't special-case its missing arg.
- Reason: path entry should never be raw typing.

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

[2026-09-15@2e8e8c7] Taller workspace tabs showing the pinned directory

- Requires: NO BLOCKERS (workspaces have a root directory: `Vtab.root_dir`)
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

---

[2026-09-28@aaa6e17f]

- Requires: NO BLOCKERS (research spike)
- Detail: Spike a terminal grid wider than the pane with horizontal scroll, vs
  the current pane-width grid. Open question: the child queries terminal size
  (ioctl/SIGWINCH); a grid wider than the visible pane would misreport width and
  programs would still wrap to the reported cols. Evaluate whether a wider grid
  is coherent (and what to report) before committing.
- Reason: view long unwrapped terminal output without the shell wrapping it.

---

[2026-09-29@19e0239] File browser — alternate views (grouped)

- Requires: NO BLOCKERS (extends `core::fs_tree::FsTree`, already headless)
- Detail: A `view` field on `FsTree` selects how `rows()` groups the recursive
  subtree into fake collapsible categories (rendered like directories, not real),
  with a header toggle to switch. Views to build:
  - **by extension** (requested): one category per extension (`.kt`,
    `.gradle.kts`, `.jpg`); expand to list every file of that type.
  - **by kind** (higher level): group extensions into Images / Video / Audio /
    Documents / Code / Archives / Data / Other — great for a Downloads folder.
  - **by git status**: Modified / Untracked / Staged / Clean categories.
  - **by modified date**: Today / This week / This month / Older buckets.
  - **by size**: tiny / small / large / huge buckets; plus a flat "largest files".
  - **recent**: flat, newest-first across the tree.
  DISAMBIGUATION for any flat/grouped view: when two shown files share a name,
  prefix each with the minimal distinguishing ancestor (grayed) — walk up from the
  leaf while the candidate segment is shared by another shown file, stop at the
  first that differs (Kotlin `util` packages sharing a/b/c climb to the module:
  `source` vs `network`). Show that segment before the file name in dim text.
- Reason: navigate by kind/status/date, not just by folder; requested + brainstorm.

---

[2026-09-29@19e0239] File browser — preview pane

- Requires: NO BLOCKERS (image first; media is a large follow-up)
- Detail: When the browser pane is wide enough (a minimum pixel width; below it,
  hide the preview rather than repositioning it to the bottom), split off a
  right-hand preview of the hovered/selected file:
  - **details** always: name, size, mtime/ctime, kind, and for a dir its entry
    count.
  - **text files**: a scrollable head of the contents (reuse the editor's read +
    row rendering).
  - **images**: decode and show the image (wgpu texture upload; new render path).
  - **video/audio**: play with an autoplay setting (`[preview] autoplay`, default
    true) — a UI toggle button flips it and writes it back to config + hot-reloads
    (like the existing editor soft-wrap toggle pattern). Media decode/playback is a
    big dependency decision (e.g. an ffmpeg/symphonia route) — spike separately.
  Build order: details + text preview → images → media. Keep the min-width gate so
  a narrow pane just shows the tree.
- Reason: Finder-style at-a-glance preview; explicitly requested (autoplay wanted
  on by default).

---

[2026-09-29@fddd044] File browser — remaining polish

- Requires: NO BLOCKERS
- Detail: (1) gitignored files should render in a grayed red (and ignored folders
  bold) — needs a per-entry "ignored" flag surfaced from the walk (currently
  `ignore` skips them unless the toggle is on; when shown, mark them). (2)
  directories could be BOLD as well as accent-coloured — needs a weight on the
  shaped span (glyphon `Attrs::weight`). (3) no header UI to SET the ext filter
  (`set_ext_filter` exists; git & recent have buttons) — add a control or an
  `ext:rs` query token. (4) date filter is a single "recent 7d" toggle — add an
  explicit range + created-vs-modified. (5) recursive fuzzy walk caps at 20k
  entries; revisit for huge roots. (6) confirm nested `.gitignore` on a real repo.
- Reason: the file-browser power tools; polish beyond the delivered basics.

---

[2026-09-29@6641e98] Plug-and-play syntax highlighting (+ Markdown)

- Requires: NO BLOCKERS
- Detail: Today only TOML highlighting is hardcoded in the editor
  (`plugins::editor::toml`). Extract a highlighter interface — a trait/registry
  keyed by extension that turns a logical line (with some cross-line state, e.g.
  inside a fenced block) into coloured spans — so file types are added as small
  modules. Live in core (headless, testable) so the editor and the browser's
  text-preview share it. Add **Markdown**; use the theme palette for colours (see
  the Markdown-formatting entry). Later: consider LSP-backed semantic highlight,
  but keep the simple rule-based path for the common case.
- Reason: expand language support over time as little modules; requested.

---

[2026-09-29@6641e98] Markdown formatting in the editor

- Requires: the syntax-highlighter interface above
- Detail: Render Markdown with style while keeping it editable. Default to a
  **mixed** style: `*italic*` shows italic AND keeps the asterisks; `**bold**`
  shows bold AND keeps both markers; headers styled but the `#` still shown (a
  green-ish `#` from the palette). A toggle button switches mixed ↔ fully-formatted
  (markers hidden). Highlight links; a Markdown feature checks whether a link/image
  target exists (local file on disk, or a reachable URL) and flags broken ones with
  error feedback. Needs glyphon `Attrs::style(Italic)`/`weight(Bold)` per span and
  inline error markers.
- Reason: nicer Markdown editing; requested.

---

[2026-09-30@7d132a83] Plugin architecture — follow-ups

- Requires: NO BLOCKERS
- Detail: DONE: `Plugin`/`View` API (`app::plugin`), immediate-mode `PaintCx`,
  `EventCx` + request queue, headless test harness; terminal, editor, and file
  browser are plugins; the picker and `<id>.new` commands come from the registry;
  files open with the first plugin that claims the path (PLUGINS.md). Remaining:
  (1) move the rest of the app's own chrome (sidebar, tab strips, palette,
  menu) onto `PaintCx` as the empty-pane picker and dir picker already are,
  retiring their per-widget buffer pools; (2) extract the API
  into its own crate for out-of-tree Rust plugins — needs commands registered
  against a trait rather than `Registry<AppState>`; (3) per-plugin config sections
  (today first-party plugins read typed fields on `core::Config`); (4) view-scoped
  commands and keybindings (e.g. editor-only chords in `[keybindings]`, palette
  entries that act on the focused view); (5) a web-browser plugin — a CEF/webview
  composites its own surface, so `PaintCx` needs a textured-quad primitive fed by a
  per-view texture (acceptable for non-hot panels, not the terminal); (6) dynamic
  loading / WASM for third-party plugins, if ever.
- Reason: finish the uniform-plugin model and open it to new kinds.
---

[2026-09-29@6641e98] Extract one shared scroll/shape pipeline

- Requires: NO BLOCKERS
- Detail: DONE: the row cache, per-frame shaping budget, and stale-row fallback
  are shared through `PaintCx::row` / `budgeted_row`. Remaining: scroll pacing is
  still per view — the terminal eases wheel input over frames, the editor snaps by
  lines, the file browser by pixels. Extract a reusable "scrollable shaped-row
  list" (viewport + easing + budgeted placement) the row-based views share, with
  view-specific hooks only where needed (e.g. the terminal's grid snapshot).
- Reason: one scroll-optimisation path; the file browser proved the win, don't fork it.
---

[2026-09-30@463e7cf] Settings — search/add from the full option list

- Requires: NO BLOCKERS
- Detail: The config no longer needs to dump every option. Instead, when the
  config file is open in the editor, offer a way to search the full option catalogue
  and insert a chosen option (with its documented comment + default) at the cursor —
  a palette-like picker over all known settings. Pairs with the backfill/doctor
  idea. Keeps the file lean while staying discoverable.
- Reason: a complete but un-cluttered settings surface.

---

[2026-09-30@463e7cf] A ghostrealm LSP (config first)

- Requires: LSP plumbing (see the plugin/syntax entries)
- Detail: Once we have LSP support, ship our own LSP that understands the config
  file: validate keys (flag unknown/misspelled options), report out-of-range or
  wrong-type values and basic TOML syntax errors non-fatally, and provide on-hover
  docs for each option (reuse the same catalogue as the settings search). Later,
  extend to command ids in `[keybindings]` (unknown id / duplicate chord) and to
  other ghostrealm file types.
- Reason: make the file-as-settings-UI safe and self-explaining.

---

[2026-09-30@463e7cf] Double-tap rework + fuzzy file-search popup

- Requires: NO BLOCKERS (reuses the FsTree browser + the picker overlay)
- Detail: Rework the double-tap triggers (currently double-Shift = palette,
  double-Ctrl = Run) toward an Android-Studio feel: double-Ctrl opens the command
  palette (fold the one-shot "Run Anything" terminal into it somehow), and
  double-Shift opens a floating fuzzy FILE search over the current workspace/root
  directory group — reuse the FsTree fuzzy filter and the floating picker overlay,
  Enter opens the file (respecting `[file_browser] open_in`). Make the triggers
  configurable (the existing `[input] double_tap_window_ms` + remappable actions).
  Later: if the root is a code repo, index code SYMBOLS (via the LSP) and search
  those instead of/along files.
- Reason: fast keyboard-only navigation; requested.

---

[2026-10-04@c994011] Floating picker — follow-ups

- Requires: NO BLOCKERS
- Detail: The picker chooses a directory, a file (`editor.open`), or where to
  save (Save As). (1) Save mode's name field only appends/backspaces: no caret
  movement, selection, or paste. (2) In save mode typing goes to the name, so the
  list can't be filtered or given a typed path — e.g. Tab to move focus between
  the name and the header field. (3) Saving over an existing file only says
  "Replace" on the button; no confirm step.
- Reason: the picker is now the app's open/save dialog; it should feel like one.
