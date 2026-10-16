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
  menu; mouse reporting to programs that enable tracking (left button, drag, hover,
  vertical wheel; Shift keeps selection/scroll local). Still to do: the plugin API
  delivers only the left button, so right/middle clicks and horizontal wheel are
  never reported; alternate scroll (mode 1007: wheel -> arrow keys on the alternate
  screen when tracking is off) is not implemented. (Click-to-position is skipped —
  not meaningful for a shell.)
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
  a small state file beside the config, not the config itself: they are
  machine-local history, and the config is the shareable dotfile.
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
  has more than one pane (avoid accidental loss) — `AppState::confirm` exists (Save
  As replace uses it); add a `Confirmed` variant for closing.
- Reason: context actions per tab type; prevent accidental loss.

---

[2026-09-15@2e8e8c7] Splits — discoverability, close and split buttons

- Requires: NO BLOCKERS
- Detail: (1) Split commands are hard to find (buried below the palette fold) — add
  default keybindings and surface them. (2) A close "x" on a pane and split
  right/left/up/down buttons (owner's words in "DISCUSSION: editor and file
  browser as one surface"). Resize handles and free ratios are done; split
  ratios are not saved between runs (nothing in the workspace tree is).
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
- Detail: DONE (rudimentary): Cmd+Z / Cmd+Shift+Z in the editor; whole-buffer
  snapshots (200 steps), typing runs and single-char delete runs coalesce,
  undoing to the saved text clears `modified`. REMAINING: (1) a reversible op
  log instead of snapshots (each step copies the whole buffer — costly for big
  files); (2) finer runs (break at word boundaries / after a pause); (3) undo in
  single-line fields (`LineEdit`); (4) history survives Save As / reload. User
  has further ideas on scope/behaviour — discuss before extending.
- Reason: basic editing expectation.

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
  - **modified**: Today / This week / This month / Older buckets by
    modification time. Lists every file; it is a grouping, not a filter.
  - **by size**: tiny / small / large / huge buckets; plus a flat "largest files".
  - **recent**: flat, newest-first across the tree.
  DISAMBIGUATION for any flat/grouped view: when two shown files share a name,
  prefix each with the minimal distinguishing ancestor (grayed) — walk up from the
  leaf while the candidate segment is shared by another shown file, stop at the
  first that differs (Kotlin `util` packages sharing a/b/c climb to the module:
  `source` vs `network`). Show that segment before the file name in dim text.
- Reason: navigate by kind/status/date, not just by folder; requested + brainstorm.

---

[2026-10-05T20:35+1300@3f581979] File browser — preview pane follow-ups

- Requires: NO BLOCKERS
- Detail: The preview (details, text head, images, audio) is in. Remaining:
  - **video**: decode + play (an ffmpeg-class dependency decision; spike
    separately). Reuse the audio controls and `PaintCx::image` per frame.
  - **opening media**: clicking an image/audio file in the browser opens it in
    the editor (the catch-all). Add an image/audio viewer plugin that claims
    those paths (the preview's loader and player are the parts to reuse).
  - **autoplay toggle**: the preview's `autoplay` button is a per-pane override
    of `[file_browser] preview_autoplay`; write it back to the config instead,
    if that is the wanted behaviour.
  - **wake, not poll**: the preview polls its loader on a 15 ms timer while a
    load is in flight. `View::refresh` (repaint without marking unread) would
    replace the poll once the loader wakes the UI (`OpenCx::waker`) and the
    floating picker's view is refreshed like a pane's.
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
  explicit range. Which time to filter by is open: the owner suspects modified
  is the one wanted and created is not useful. (5) recursive fuzzy walk caps at 20k
  entries; revisit for huge roots. (6) confirm nested `.gitignore` on a real repo.
- Reason: the file-browser power tools; polish beyond the delivered basics.

---

[2026-09-29@6641e98] Syntax highlighting — more languages

- Requires: NO BLOCKERS
- Detail: The registry (`ghostrealm_core::highlight`) has TOML and Markdown. Add
  more as small `Highlighter` modules (JSON, YAML, Rust, shell, ...); each is one
  line in `highlight::ALL`, declares its comment tokens (`Highlighter::comment`)
  and drops its `highlight::LANGUAGES` entry. Later: consider LSP-backed semantic highlight, but
  keep the rule-based path for the common case. Markdown has no setext headings,
  indented code, or YAML front matter (they need line-number or neighbouring-line
  context). The direction for the main languages (tree-sitter, hand-written, or
  a unified keyword API) is open: see "DISCUSSION: syntax highlighting
  direction".
- Reason: expand language support over time as little modules; requested.

---

[2026-09-29@6641e98] Markdown rich text — follow-ups

- Requires: NO BLOCKERS
- Detail: DONE: **mixed** mode. Strong/headings bold, emphasis italic, link text
  underlined, inline code in a tinted box, fenced blocks on a full-width tint
  with the body highlighted in the fence's language (a line-local highlighter
  such as `toml`); the markers stay visible, editable, and dimmed. The style
  rides in `RunStyle`s per span through the row cache and `PaintCx`
  (PLUGINS.md); rows with no styled span carry none. Remaining:
  - A toggle button switches mixed <-> fully-formatted (markers hidden; needs
    cursor-column mapping over hidden text).
  - A Markdown feature checks whether a link/image target exists (a local file
    on disk; a reachable URL needs a network decision) and flags broken ones
    with inline error markers.
  - Code blocks: a copy button and, for shell-flavoured blocks, a run button in
    the left gutter for each runnable command; an LSP for the fence's language;
    highlighting a fence in a stateful language (Markdown in Markdown) needs
    the inner state carried too; see "DISCUSSION: syntax highlighting
    direction".
  - Links: look and feel (colours) of internal and external links, to discuss.
  - Nesting: the highlighter gives each char one token, so emphasis inside
    strong (or a link or heading) loses the outer style; needs a token set per
    char, or style flags on spans.
  - The file browser's preview colours Markdown but does not draw the styles.
  - Bold has no synthetic fallback: a monospace family without a bold face
    shows the nearest weight (italic is skewed by the shaper).
  - From GitHub issue #1 (2026-10-16@ad13ff26):
    - Code look: inline code and fenced blocks want a rounded box with a
      defined border and a calmer colour scheme; the tint (`RunStyle::BOX`,
      `theme::CODE_BG`) reads as a warning. Compare GitHub and Android Studio.
    - Headings differ only in weight; they should differ in size. Needs a font
      size per span, so variable row heights: an architectural change to the
      row cache, `PaintCx` rows, and every view's row-to-y mapping.
    - Three view modes per file, switched by a ribbon button: Preview
      (rendered, read-only), Source (plain, editable), Integrated (source chars
      with the formatting, editable; Markdown's mixed mode). Extend to other
      languages: TOML `[table]`s as large headers (they render bold today
      because TOML and Markdown share `Token::Heading`, which `syntax.rs` maps
      to bold; the owner likes it, so keep it as the seed); Kotlin top-level
      classes as H1 shrinking with nesting, functions and non-local properties
      as H3/H5; a Kotlin Preview could render doc comments and generated
      diagrams (relationships, method flow).
- Owner, verbatim:
  > Okay, yeah, I still really want the bold and italics and markdown, so we
  > better plumb that. Try and make it efficient, though. Like, I don't think
  > you need to send all that information if none of those are being used, only
  > when they are being used. Also note that we need to add single grave and
  > triple grave code blocks. So single grave, obviously, like inline, and we've
  > just got little square versus triple grave where we've got like a full on
  > code block thing going on. And maybe a copy button. And maybe a run button
  > in the left gutter for individual commands that are actually runnable. Well,
  > I guess that only applies to shell flavored code blocks. Oh, and code blocks
  > will need to bring in syntax highlighting and maybe LSPs for the relevant
  > language of whatever language is defined after the triple grave, the start
  > of the code block. We probably also want to use those sorts of highlighting
  > things for some other things like internal links or external links, stuff
  > like that. Though we can discuss the look and feel of that later.
- Reason: nicer Markdown editing; requested, and wanted soon.

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
  against a trait rather than `Registry<AppState>`; (3) per-plugin typed config — each
  plugin already documents its sections (`Plugin::config_sections`), but the typed
  fields still live on `core::Config`; a plugin should deserialize its own section; (4) view-scoped
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
  save (Save As). In save mode typing goes to the name, so the list can't be
  filtered or given a typed path — e.g. Tab to move focus between the name and
  the header field. While a confirmation is open the picker is hidden: overlay
  text draws above all overlay quads, so modals can't stack (needs per-modal
  layers in `Frame`).
- Reason: the picker is now the app's open/save dialog; it should feel like one.

---

[2026-10-13@13e2bda] Editor — section outline (jump to a header)

- Requires: per-language header extraction — a hook on the `Highlighter` trait
  (`ghostrealm_core::highlight`) for simple formats; the LSP (document symbols)
  for code
- Detail: Not a minimap: a list of the file's section headers that jumps to one.
  Headers per language: TOML `[table]` / `[[array]]`, Markdown `#` headings, code
  symbols once an LSP is wired. Clicking a header scrolls so it sits mid-screen.
  Undecided: where the list lives (side panel, ribbon dropdown, palette-like
  popup), nesting, whether it tracks the cursor's current section.
  GitHub issue #1 proposes the Claude Code app's sticky table of contents:
  collapsed in a corner, expands on hover, click to jump, unaffected by scroll.
- Reason: navigate long files (the config included) by structure.

---

[2026-10-13@13e2bda] Editor — inline git change highlights

- Requires: line-level diff hunks (`core::line_diff` only counts today)
- Detail: While the ribbon statistic shows changes (or via a toggle), tint line
  backgrounds like the cursor-line highlight (both can apply): green for added
  lines, red for changed, with the changed characters in a more vibrant shade
  (an intra-line diff). Deleted lines have no row in the buffer; options: (a) a
  red marker in the gutter between the surrounding lines (hover/click shows the
  removed text), (b) read-only phantom rows drawn between real lines — truer to
  the diff, but every row-to-line mapping (clicks, wrap, scroll) must skip them.
  Start with (a). Cost: compute the line hunks once per edit (cached on the edit
  count, like the counts), keep a per-line status for O(1) lookup, and run the
  intra-line diff only for visible changed rows, cached per line.
- Approach: native, not a "git LSP" (LSP has no primitives for line tints,
  gutter markers, or phantom rows; git isn't per-language; per-keystroke IPC
  would lag an in-process diff that costs microseconds). Split it in two:
  (1) a decoration layer on the editor — line/range background tints, gutter
  markers, later phantom rows — painted for visible rows only; (2) providers
  that produce decorations for a buffer. Git is the first provider (HEAD text +
  line hunks + intra-line diff); LSP diagnostics and the section outline can
  feed the same layer later. The editor never knows about git.
- Reason: see what changed since the last commit in place, not just how much.

---

[2026-10-05@7654ce1] Language services — follow-ups

- Requires: NO BLOCKERS
- Detail: DONE: `crates/lsp` (first-party `LanguageServer` + `serve`, the editor's
  `Client` for in-process or spawned servers), the config server (diagnostics from
  the checker, hover from the catalogue; also `ghostrealm lsp config` on stdio),
  and the editor's underlines and hover popup. REMAINING, roughly in order:
  1. Completion (`textDocument/completion`): section names in headers, option
     keys per section, enum values, command ids in `[keybindings]`; plus the
     editor's completion popup. With a slim config file this is how options are
     discovered (today: hover a section header for its option list).
  2. External servers: a config section mapping file types to a server command
     (e.g. a Kotlin server for `.kt`), spawned via `Client::spawn` with the
     workspace root. Share one process between a workspace's documents of the
     same language, shutting it down when the last one closes.
  3. Diagnostics beyond the underline: a gutter marker, and a way to jump
     between them.
  4. Markdown hover contents (the client advertises plain text only).
- Reason: finish the config file as the settings UI, then open the editor to
  existing language servers instead of building language support in-tree.

---

[2026-10-05@7654ce1] Colour swatches in the gutter + colour picker

- Requires: flexible colour values with alpha (the colour-input entry above)
- Detail: The config server answers `textDocument/documentColor`; the editor
  draws a small square in the gutter, filled with the colour each line's value
  resolves to. Clicking the square opens a colour picker (a built-in plugin)
  that writes the chosen value back via `textDocument/colorPresentation`.
- Reason: see and pick colours in place; requested (picker later than the
  language service itself).

---

[2026-10-13@939ddd4] Shortcuts sheet for the focused pane (title-bar button)

- Requires: views that declare their own chords (plugin follow-up (4),
  view-scoped keybindings); editing needs config write-back
- Detail: A button in the title bar beside the sidebar toggle opens a sheet of
  every shortcut that applies to the focused pane's view (one global button,
  not one per pane, since splits show several views). Sections, top to bottom:
  1. Global: app chords from `[keybindings]` (splits, workspaces, panes).
  2. Text editing: the platform `Keymap` (`plugin::text_edit`) — word/line
     motions, deletes, clipboard, undo; fixed per platform today.
  3. Everything else: the view's own chords (e.g. the editor's Cmd+S /
     Cmd+Shift+S) and any custom bindings that fit neither group above.
  Later, let the sheet edit bindings: a global chord writes back to
  `[keybindings]` (flagging clashes, as the config checker does).
  Open: where the button lives without the macOS title-bar strip, and whether
  the text keymap becomes configurable.
- Reason: discover (and change) shortcuts where you use them; idea, not
  committed to.

---

[2026-10-05@04794e1] Web surface — follow-ups

- Requires: NO BLOCKERS
- Detail: The basic CEF web surface is in (toolbar, URL bar, page input, agent
  commands). REMAINING: (1) verify on a real desktop session — page drawing,
  HiDPI sharpness, cursor shapes, typing (incl. Option/dead keys) and
  Cmd+C/V/X/A in pages; (2) IME composition (`ime_set_composition`); (3) file
  upload dialogs, downloads, JS alert/confirm dialogs (currently CEF defaults /
  unhandled); (4) drag and drop; (5) restart the host automatically after a crash
  (today a dead host is noticed on the next resize/open); (6) Linux bundle is
  untested; (7) devtools (`show_dev_tools`, or the remote debugging port) as a
  command; (8) don't render a page nobody is watching: skip painting when the
  page is not visible to the user (hidden tab or workspace) AND no agent is
  actively watching it. Owner (round 3): "Is it visible to the user [...] And is
  it visible to an agent? Is an agent actively watching it? If it's not, then we
  don't need to do the whole render stuff."
- Reason: a usable everyday browser pane.

---

[2026-10-05@93248d9] Agent channel → the running app (+ MCP server)

- Requires: NO BLOCKERS
- Detail: `ghostrealm agent` runs its own headless AppState; it cannot see or drive
  the panes in an open window. Serve the same protocol from the running app over a
  local socket (per-window path, exported to child shells as an env var), and wrap
  it as an MCP server (stdio shim that connects to that socket) so an agent in a
  terminal tab controls *this* window's workspaces, terminals, and web panes.
  Scope agent access per the existing "hide from agent" idea.
- Web profiles, proposal (a consequence of the profile lock, not an owner
  decision): Chromium locks a profile to one process, so the web host would be
  owned by the running app and agents would connect through this socket instead
  of spawning their own host (see "Web — profiles").
- Reason: agent control of the live UI is the point of the agent channel.

---

[2026-10-05@93248d9] Web — structured page text + ref-based actions for agents

- Requires: NO BLOCKERS (builds on `web.eval`)
- Detail: `get surface` returns flat page text. Add a compact structured outline
  (headings, links, buttons, inputs, landmarks, with stable element refs), built by
  an injected script (or CEF's accessibility tree), plus `web.click {ref}` /
  `web.type {ref, text}` / `web.select`, so an agent can act on a page without
  pixels or guessed selectors.
- Reason: agents read and act on pages reliably and cheaply without screenshots.

---

[2026-10-05@93248d9] Agent screenshot command

- Requires: agent channel → running app (for terminal/chrome captures)
- Detail: `get what=screenshot` (surface id, or the whole window) returning a PNG
  (base64; an MCP image result). Web surfaces can encode their latest shared frame
  directly; terminals/chrome need an offscreen render of the scene.
- Reason: some questions are visual (layout, rendering bugs, canvases).

---

[2026-10-05@93248d9] Web — preload on link hover and URL-bar typing

- Requires: NO BLOCKERS
- Detail: (1) Link hover: inject speculation rules (prefetch/prerender with
  `eagerness: moderate`, Chromium's own hover heuristic) on page load. (2) URL bar:
  after a short typing pause, when the input resolves to an address (not a
  search), preconnect, and optionally prerender in a hidden browser that is swapped
  in on Enter. Configurable (`[web] preload = off | connect | prerender`); never
  prerender searches, and accept that prerendering issues real GET requests
  (side effects, analytics, intent leaked to the site).
- Reason: navigation feels instant.

---

[2026-10-05@93248d9] Web — GPU (accelerated) off-screen rendering

- Requires: verify GPU compositing paints on a real display (it produced no
  frames headless, hence `--disable-gpu-compositing`)
- Detail: Replace CPU frames with CEF shared textures (`shared_texture_enabled`,
  `on_accelerated_paint`): an IOSurface on macOS, a DMA-BUF on Linux. Pass the
  handle to the app (IOSurface mach port; DMA-BUF fd over a Unix socket) and import
  it as a wgpu texture (cef-rs `accelerated_osr` already imports into wgpu 30).
  Removes the raster→shm→upload copies; keeps process isolation and unified
  compositing. Keep the CPU path as a fallback.
- Reason: video, scrolling, and animation at HiDPI without burning CPU.

---

[2026-10-05@93248d9] Web — exit the host when no web surfaces remain

- Requires: NO BLOCKERS
- Detail: The host is spawned on first use and lives until the app quits. Close it
  (stdin EOF) a little after the last web surface closes; it respawns on demand.
- Reason: an idle Chromium browser + GPU process hold a few hundred MB.

---

[2026-10-13T19:46+1300@d292715] PRIORITY: User-visible error surface

- Requires: a design (prompt the user about it)
- Detail: Errors reach stderr only: a failed Save As, a refused open of an
  unreadable or non-UTF-8 file, config load findings. (Web host failures are
  shown in the web pane and reported by `web.new`.) Design
  where they show (toast, status line, inbox, settings status). See
  [stack-review-findings.md](stack-review-findings.md) item 5 (silent Save As and
  open failures) and [stack-review-findings-2.md](stack-review-findings-2.md)
  item 15 (load-time findings go only to stderr).
- Reason: the user cannot see why an action did nothing.

---

[2026-10-13T19:46+1300@d292715] Terminal: text-editing keys inside full-screen programs (decision pending)

- Requires: a decision from the owner (see "Decision session")
- Detail: Option/Cmd word and line motions and deletes are sent to the program
  as readline/emacs bytes (`ESC b`, `^A`, ...). Inside a full-screen program
  (alternate screen) such as Vim, those bytes get Vim's meaning, not the editing
  action the key stands for. The owner wants the same action in Vim.
- Detail: Proposal: a translation layer. In the alternate screen, send the
  standard xterm key sequences instead (Option+Left -> `CSI 1;3D`, Cmd+Left ->
  Home), which Vim can map in insert mode. Vim's normal mode has no "word left"
  for those keys without mappings, so the layer may need to ship suggested
  mappings. Alternative: turn the translation off in full-screen programs.
  Either way, a TOML boolean controls it. See
  [stack-review-findings-2.md](stack-review-findings-2.md) item 7.
- Owner, verbatim:
  > I'm looking at number seven, and you're saying that my key bindings won't do
  > the same thing in Vim. That sounds like a problem to me. I think they should
  > do the same thing in Vim, not do whatever Vim's key bindings say to do. So I
  > guess we need a translation layer there. Because I do actually use Vim.
  > Alternatively, maybe we just consider turning it off.
- Reason: the translated keys must do the same thing everywhere the owner types,
  including Vim.

---

[2026-10-13T19:46+1300@d292715] File browser: should the preview follow hover?

- Requires: the user tries the behaviour before deciding
- Detail: Decide whether the preview follows hover, the selection, or a pinned
  row. See [stack-review-findings-2.md](stack-review-findings-2.md) item 34.
- Reason: moving the pointer to the preview's controls crosses rows and switches
  the subject.

---

[2026-10-13T19:46+1300@d292715] Reduce dependencies

- Requires: NO BLOCKERS (later work, not now)
- Detail: Audit the dependency tree (`image`, `rodio`, `cef`,
  `lsp-types`/`lsp-server`, `crossbeam-channel`, `toml`, `libc`, ...) and replace
  with small in-house implementations where reasonable. `image` and `rodio` are
  wrapped by `crate::media` (`ImageDecoder`, `AudioBackend`), so replacing
  either is local to `media::image::image_crate` / `media::audio::rodio_backend`.
- Reason: fewer external dependencies to carry and update.

---

[2026-10-13T19:46+1300@d292715] Keybindings: recommended defaults

- Requires: NO BLOCKERS
- Detail: Keep a list of recommended bindings per command. Later, add a way to
  inject a recommended binding into the user's config on request (e.g. from the
  palette or the shortcuts sheet). Never automatic: `[keybindings]` binds only
  what the file lists.
- Reason: with no built-in fallback, users need a way to discover and add the
  recommended chords.

---

[2026-10-13T19:46+1300@d292715] Decision session: open "Decision needed" items

- Requires: a session with the user
- Detail: Go through the items still open and check the decisions are
  consistent with each other:
  - Text-editing keys inside Vim and other full-screen programs: the
    translation-layer proposal vs turning the translation off (findings-2 item
    7; TODO "Terminal: text-editing keys inside full-screen programs").
  - Keybinding ids without quotes: confirm flattening nested tables (TODO
    "Keybinding ids without quotes").
  - [stack-review-findings-2.md](stack-review-findings-2.md) items 15 (load-time
    findings), 24 (stuck mount), 34 (preview follows hover; the owner will try
    it first), 36 (unbracketed paste),
    and the "To confirm" note in [stack-review-findings.md](stack-review-findings.md).
  - The open design items: the profile scope controls and picker (TODO "Web —
    profiles"), syntax highlighting direction, editor and file browser as one
    surface.
  Decided in the review rounds and no longer open: Chromium sandbox on, web
  profile location, pop-up handling, shell exit, Delete = Trash, GitHub
  permalinks (future), the file-browser date view naming.
- Reason: several decisions are pending and may interact.

---

> The entries below are from the project owner's review of build 914539d
> (2026-10-13). All are HIGH PRIORITY (quality of life). Each was checked
> against e9617b8; notes that were already fixed are not listed. The "Small
> fixes" entries are actionable now; the "Discussion" entries need a design
> session before any code.

---

[2026-10-13T20:25+1300@e9617b8] HIGHEST web priority: build the web host with the normal build

- Requires: a choice between the options below (the CEF download size is the cost)
- Detail: The CEF host bundle is built only by `mise run web`. After a normal
  `cargo build` or `cargo run --release` the web surface reports "web host not
  found: build it with `mise run web`", so the browser is always down until a
  second command is run. The owner wants `cargo build` and `cargo run
  --release` to build the web host too, skipped when nothing changed (cached).
  Priority: the highest item the next time web work starts; other work may go
  first.
- Why it is a separate build:
  - The host is a separate executable (DECISION.md, Web surface): it must be
    packaged as a macOS `.app` bundle with the CEF framework and helper apps,
    because Chromium requires that layout. A bundle binary
    (`ghostrealm-web-bundle`, behind the `bundle` feature) produces it and also
    downloads CEF (~250 MB).
  - Cargo has no post-build step that packages another binary, and the host
    crate is excluded from the workspace's `default-members`, so `cargo build`
    never touches it.
  - `mise run web` is the repo's task-runner wrapper (`mise.toml`, which also
    pins Zig, CMake, and Ninja) around two cargo commands: build the host with
    `--features bundle`, then run the bundler. It is not a Rust-ecosystem
    command; it exists because cargo cannot express the packaging step.
- Options:
  - The app's `build.rs` drives the bundle step with change detection. A plain
    `cargo run --release` then yields a working browser. Costs: a nested cargo
    invocation (slower, build-directory lock contention), and every first build
    needs CMake + Ninja and the CEF download.
  - A cargo alias or `xtask` for build/run. Does not help plain `cargo run
    --release`, which is what the owner uses.
  - The app builds or fetches the bundle on first use of a web surface (with
    progress shown), so only users who open a browser pay for it. The owner
    notes they want the browser anyway, so the saving is theoretical for them.
  - Either way, keep `[web] host` / `$GHOSTREALM_WEB_HOST` as overrides.
- Owner, verbatim:
  > I definitely think that cargo build or cargo run, etc., should also be
  > triggering the web browser build. I'm not actually sure why that's a
  > separate build, to be honest. You might have to expand on that. Though I do,
  > I guess I like the idea of it being a plugin that I could disable. But
  > what's the point? I do actually want the web browser, so it might as well be
  > in there. So yeah, we should absolutely make that part of the normal build
  > process and maybe just like skip it if there's been no changes. We use the
  > cached version if we don't need to build it.
  >
  > Why is the build command for the web module mise run web anyway? That
  > doesn't really make sense to me. I've just been using cargo run dash dash
  > release the whole time. Like, why is it mise rather than a Rust ecosystem
  > command? I think patching the web build into the rest of the build command
  > is the highest priority the next time we're touching web stuff. So we can do
  > other stuff first, but when we get to web, fixing that is very important.
- Reason: a feature that needs a separate manual build step looks broken.

---

[2026-10-14@7364079] File and folder context menu — remaining

- Requires: NO BLOCKERS (each item below is independent)
- Detail: The menu exists (right-click in the file browser, the editor's file
  name in its ribbon; README, PLUGINS.md). Not built:
  - **Undo / Redo** of file operations (Zed's Edit section). Needs an operation
    log with an inverse for create, duplicate, paste, rename, move, and Trash
    (move back from the Trash). Left out rather than half-done.
  - **Finder interop for Cut / Copy / Paste**: the file clipboard is the app's
    own; the system pasteboard (file URLs) would let Paste work with Finder.
  - **Delete on other platforms**: only macOS moves to the Trash
    (`NSFileManager trashItemAtURL`); elsewhere it reports "not supported" and
    never deletes permanently. The `trash` crate or the freedesktop spec would
    cover Linux.
  - **Editor tabs**: the menu opens from the single-tab ribbon's file name; with a
    tab strip the file name is a tab, which has no file menu yet.
  - **Cmd+Backspace** on the file browser's selection to delete (from the old
    "Delete files" entry).
  - New File / New Folder create the entry and do not open it.
  - Failures (name taken, nothing to paste, Trash refused) reach stderr only;
    see "User-visible error surface".
- Not in the menu: **GitHub permalinks** (see "GitHub permalinks"), Open
  Markdown Preview (Markdown is formatted in the editor).
- Reason: the first menu covers the owner's list; these are the edges.

---

[2026-10-13T20:25+1300@e9617b8] Editor and file browser as one surface

- Requires: the implementation plan (written separately)
- Decided (2026-10-16@ad13ff26, GitHub issue #1): clone the Claude Code app's
  file browser and editor: one pinnable file sidebar shared by every tab, file
  tabs (single-click opens an ephemeral preview tab, double-click a real one),
  and its filter/search box, plus the other things the issue says the owner
  likes. The existing preview maybe stays for non-text files (images, audio).
  The design: the Files surface owns its own sidebar and inner file-tab row;
  documents are child views.
- Detail: The candidate shapes (the first was chosen):
  - One tab, Claude Code style: a show/hide file tree, a filter box (with a
    "? to search in files" hint), and the document.
  - The file browser hosts pluggable viewers (image, audio, text preview/editor);
    the image and audio ones relate to "File browser — preview pane follow-ups".
  Related, for the plan to cover:
  - **File-browser pin**: a pin icon. Pinned: opening a file puts the editor in
    a new tab or split. Unpinned: an integrated editor inside the browser (a
    back button, or collapse/expand of the browser). Maybe pin left / pin right
    / unpin. Maybe text previews keep focus on the browser.
  - **Markdown**: shown formatted, with the markup symbols still visible and
    editable. This is the "mixed" mode in "Markdown rich text — follow-ups"; the mixed mode is done.
  - **Splits**: a close "x" on a pane, and split right/left/up/down buttons.
- Reason: owner review; the file browser and editor overlap, and the choice
  decides several smaller features.
- Owner, verbatim (the decision):
  > What I mean is that the way that the Claude Code file browser does it that
  > I've highlighted in the issue there is how I think I would like it to work.
  > That seems to be my conclusion on the um, merging of the editor and the file
  > browser. I think um, I think that might also invalidate the preview split
  > stuff. Um, because now we're talking more about a sidebar rather than a
  > preview. Um, although maybe the preview still exists for non-text files.
  > Um, and also, like, keeping some parts of the preview, stuff, but um, yeah,
  > we want to move more towards this kind of integrated with different file
  > tabs, I think. Basically, I'm I want you to mostly clone what Claude Code
  > did, and make it look very much like that, apart from a few things. Like,
  > all of those things in the issue that I've pointed out, that I like, that
  > means I would like you to add those things, or do those things.
- Owner, verbatim (the design):
  > Actually maybe the editor and file browser should just be the same thing???
  > I've also attached some pictures. One of them is how Claude Code shows a
  > file browser and the document in a single tab. So you can see up the top
  > left there, we've got a show hide for the file browser. And there's a filter
  > or question mark to search in files box. And I just, I kind of like the way
  > that the markdown file is rendered as well. Although I personally would have
  > some changes there. I like to see which symbols are being used for the
  > markdown formatting specifically. So I don't want like a clean document. I
  > want like a formatted, or I, I want a document like this where it shows the
  > formatting but at the same time the actual symbols are still part of the
  > document and still editable. In any case, so yeah, I think maybe we're still
  > gonna build on this idea of like maybe the file editor and the file browser
  > should just be the same thing and they're different parts of the same thing.
  > Or maybe like the file browser has different things that plug into it. Like
  > we've already got our image preview and our audio preview. Maybe there's a
  > text preview slash editor thing. And maybe it works out that way. I don't
  > know. I don't think we should do this just yet. Just I want it there on the
  > to-do, fairly high priority to work through this together and understand
  > where we want to take this design.
- Owner, verbatim (the pin):
  > Add a pin icon to file browser. When pinned, open editor in new tab or
  > split? When unpinned, open integrated editor (maybe it has a back button or
  > a collapse/expand button for the file browser). maybe even pin left, pin
  > right, unpin? Maybe that's how we do the previews for text files? we just
  > stay focused on the browser?
- Owner, verbatim (splits):
  > Splits probably need an 'x' icon? And maybe also a split right/left/up/down
  > button(s)?

---

[2026-10-13T20:25+1300@e9617b8] DISCUSSION: settings regenerate / cleanup / reset-to-default

- Requires: a design session with the owner
- Detail: Ideas: a command to regenerate `config.toml`; a cleanup command; a
  reset-to-default command that first shows a popup highlighting the options
  that differ from their defaults. Needs config write-back and a diff of the
  file against the catalogue defaults.
- Reason: owner review; the config file is the settings UI.
- Owner, verbatim:
  > Maybe a regenerate settings.toml command? / a cleanup command? / or a reset
  > to default command (which maybe highlights which options are not default
  > before resetting in a popup confirmation?)

---

[2026-10-13T20:25+1300@e9617b8] DISCUSSION: Markdown language support (LSP, formatting)

- Requires: NO BLOCKERS (language-services follow-up 2 covers external servers)
- Detail: Syntax highlighting for Markdown exists (`ghostrealm_core::highlight`).
  Remaining: an LSP for Markdown and formatting. Decide between an external
  server (see "Language services — follow-ups", item 2) and in-tree support.
- Reason: owner review.

---

[2026-10-13T20:25+1300@e9617b8] Keybinding ids without quotes: flatten nested tables

- Requires: confirmation in the decision session (a small change)
- Detail: Action ids in `[keybindings]` are quoted (`"pane.close" = "cmd+w"`)
  because TOML parses an unquoted dotted key as nested tables (`web` ->
  `focus_url`). The owner asks to accept `web.focus_url = "cmd+l"` unquoted,
  without per-scope tables.
- Proposal: the loader flattens nested tables under `[keybindings]` back into
  dotted ids. The quoted form, the unquoted dotted form, and a
  `[keybindings.web]` table with `focus_url = "cmd+l"` then all work, with no
  format break: existing files keep working, so no migration or versioned
  format. The config checker and the config language server must treat the
  three spellings the same.
- Caveat: an id cannot be both a leaf and a prefix of another id (a `split`
  command next to `split.leftright`); check the command ids and report a clash
  as a config finding. Chords on the right stay quoted strings (they contain
  `+` and may contain spaces).
- Related: `[keybindings.<scope>]` tables can also give view-scoped bindings a
  home (plugin follow-up (4)).
- Owner, verbatim:
  > Is it possible to just do web.focusurl without the speech marks, even
  > without doing keybindings.web? Like, I don't see why you couldn't just fix
  > the parser or something and not do that. That just seems dumb. There's no,
  > like, special characters like a space that justify a set of quotation marks
  > on the left hand side, I think. Though I think it makes sense for the chord
  > on the right hand side.
- Reason: the quotes are noise, not a technical requirement; the owner asked
  why they are there.

---

> The entries below are from the owner's second review round (2026-10-13). The
> owner asked that ideas be written down with what they actually said, so each
> entry carries the verbatim quote (spoken, lightly cleaned of filler words).

---

[2026-10-13T20:45+1300@6bc650f] DISCUSSION: syntax highlighting direction

- Requires: a design session with the owner; do not implement yet
- Detail: Explore a generic tree-sitter highlighter. The owner's main languages:
  Kotlin, Gradle (Kotlin DSL), Java; then Swift and Rust; then web languages.
  Open: a hand-written highlighter per main language (languages gain keywords
  over time, which a hand-written one must track), or a unified API for
  identifying a language's keywords, possibly backed by tree-sitter.
- Detail: Markdown keeps its own highlighter, because it renders, formats, and
  shows the syntax at once ("Markdown rich text — follow-ups"). Other rich formats (HTML headers, bold) may want the same;
  undecided, since HTML is far denser. TOML moves to tree-sitter only if that
  beats the hand-written one.
- Context: the shared `Token` vocabulary in `ghostrealm_core::highlight` mixes
  generic categories (Key, String, Number, Comment) with Markdown-only ones
  (Strong, Emphasis, Link, Url, Quote, Marker). With many languages it needs
  general categories (keyword, type, function, operator, ..., like tree-sitter's
  capture names). Bold and italic are styles, not colours, so the vocabulary may
  split into syntax tokens and rich-text styles. That is the weak point of the
  Markdown-specific labels the owner asked about; if Markdown stays a separate
  highlighter, the split is what lets it still run through the same API.
- Related: per-language comment tokens for the editor's comment toggle live in
  the same per-language registry (`Highlighter::comment`). Fenced code blocks in Markdown use these
  highlighters.
- Owner, verbatim:
  > Oh, okay, I misunderstood you about highlighting in that they were the same
  > thing. We might need to explore a generic syntax highlighter using tree
  > sitter a bit more. Not implementing that just yet. So I'm thinking like my
  > main languages, I might hard code one specifically for that language, unless
  > there's a good reason why I shouldn't. Like I guess maybe the language
  > evolves over time, so there's new keywords or something. Or we can make a
  > unified API for like identifying which keywords there are. And then, yeah,
  > maybe that does use tree sitter. Specifically for Markdown, though, I think
  > Markdown deserves its own syntax highlighter just because of the way that I
  > want it to both render and format and show the syntax all at the same time,
  > which is a bit different. Oh, but you could argue that maybe we want that
  > for other rich formats, such as HTML as well. At least for like headers and
  > bold and whatnot. Then again, maybe not. HTML is far more complicated and
  > far more dense. I'm not sure if you would use the tree sitter generic thing
  > for Toml. Maybe. I don't see why you wouldn't. We'll think about that one
  > more. It would have to be an improvement on the Toml one that's hard-coded,
  > I think. My main language is Kotlin, so I would be looking at primarily
  > doing like Kotlin, Gradle, Java, syntax highlighters, or maybe Swift and
  > Rust because we do that a bit too. And then I guess there's also web. You're
  > going to have to explain to me the weak point with markdown specific labels,
  > I think. But it might not be a problem if we're keeping markdown separate
  > because rendered markdown is its own kind of fancy thing, I think. Mind you,
  > it's probably still running through the same API, isn't it? We can discuss
  > more.
- Reason: decide the highlighting architecture before adding many languages.

---

[2026-10-13T20:45+1300@6bc650f] Web — enable the Chromium sandbox

- Requires: NO BLOCKERS
- Detail: Decided (DECISION.md, Web surface / security): enable the sandbox, with
  no runtime flag to turn it off (add one only if debugging ever needs it).
  Work: link `cef_sandbox` into the macOS helper bundles (the host passes
  `no_sandbox: 1`, `crates/web-host/src/main.rs`); on Linux use Chromium's
  setuid or namespace sandbox. Verify pages still load and the helpers still
  launch with the bundle layout. See
  [stack-review-findings-2.md](stack-review-findings-2.md) item 21.
- Owner, verbatim:
  > Definitely, I think, turn on the Chromium sandbox. I'm not even sure if we
  > should have the ability to turn it off. If we need to turn it off in the
  > future, maybe we can just program it to be able to turn off then. I don't
  > think we need to have a flag or anything for this. No real advantage to
  > turning it off, except for maybe debugging.
- Reason: a renderer exploit otherwise runs with the user's rights; the browser
  loads arbitrary pages and agents drive it.

---

[2026-10-13T20:45+1300@6bc650f] Web — profiles: picker, human-only vs agent, per-workspace scope

- Requires: a design session for the picker and the scope controls; the agent
  channel → running app entry for the shared host
- Detail: Decided: the profile lives in Application Support, not
  `~/Library/Caches/ghostrealm/web` (DECISION.md, Web surface / profiles).
- Detail: Profile model:
  - A profile picker in the browser and the GUI.
  - Human-only profiles that agents cannot reach structurally: the agent channel
    has no path to their cookies or tokens. The isolation is enforced by the
    host and channel, not by instructions.
  - Agent profiles, shared between agents.
  - A per-profile scope, set by the agent or the human: private to one
    workspace, or shared across workspaces. Humans can scope their own profiles
    the same way.
- Proposal (a consequence of the profile lock, not an owner decision): Chromium
  locks a profile to one process, so the host would be owned by the running app
  and agents would connect through it ("Agent channel → the running app"),
  not by each process spawning its own host. See
  [stack-review-findings-2.md](stack-review-findings-2.md) item 19.
- Owner, verbatim:
  > It sounds like we should move the profile to application support regardless.
  > Given that we have multiple profile support, I actually think maybe we
  > should take advantage of that. We should have a profile picker in the
  > browser and like in the GUI. And there will be one profile that is human
  > only. And we've got to make it so that it's impossible for the agent to
  > access that. And then we can have multiple other profiles, but those other
  > profiles, or single profile, and those can be agent controlled and shared
  > between the agents. Maybe even shared between workspaces, maybe not shared
  > between workspaces. Maybe there's some kind of privacy control that an agent
  > can set. So an agent in one workspace can say, this is private to my
  > workspace. No other workspace can dip into it. And maybe it can also say,
  > oh, this is public. I can share this with other workspaces. And actually,
  > maybe there shouldn't only be one human-only profile. Maybe the human also
  > has the private to this workspace or shared among all workspaces option. But
  > the human ones must be impossible to interact with via the agent. And the
  > agent must not be able to get the tokens and stuff. We want this kind of
  > isolation built in and structural, not markdown prayers or rules.
- Reason: agents browse logged-in sites; the human's sessions must be
  unreachable by construction.

---

[2026-10-13T20:45+1300@6bc650f] PRIORITY (web): isolated-world `web.eval`

- Requires: NO BLOCKERS (a CEF render-process handler)
- Detail: `web.eval` runs in the page's main JavaScript world, so a page can
  redefine functions (`console.log`, `eval`, `JSON.stringify`) and lie to the
  agent about what it finds. Run it in an isolated world, as Chrome extension
  content scripts do; in CEF that needs a render-process handler. Priority:
  high within the web work, after some quality-of-life items. See
  [stack-review-findings-2.md](stack-review-findings-2.md) item 18.
- Context: an agent-driven browser is a prompt-injection and malicious-use risk.
  The owner asked whether agents mostly browse localhost. They will often open
  localhost dev servers, but also docs, GitHub, and search results, which are
  arbitrary content, so the risk stands.
- Owner, verbatim:
  > Okay, it sounds like we need to put this isolated world thing on the to-do
  > as a fairly high priority for the sake of security. High priority in terms
  > of web stuff, although maybe some quality of life stuff will try and come
  > before that. Mainly because agents can control the browsers and therefore
  > there is clearly a prompt injection risk or a malicious use case risk.
  > Having said that, we are most likely going to be running localhost stuff
  > when we're doing stuff in the browser, right? Most of the time? Maybe not.
  > I'm not sure, actually.
- Reason: the agent must be able to trust what it reads back from a page.

---

[2026-10-13T20:45+1300@6bc650f] Web — pop-ups: floating window for humans, hidden for agents

- Requires: NO BLOCKERS
- Detail: Decided (DECISION.md, UX principle / agent-operated surfaces).
  Pop-ups currently load in place and lose `window.opener`, which breaks OAuth
  and payment flows (`on_before_popup`, `crates/web-host/src/browser.rs`;
  [stack-review-findings-2.md](stack-review-findings-2.md) item 31).
  - Human-triggered: open as a floating pop-up window on top of the whole app,
    with the original tab kept alive. Fallback if that is not feasible: a new
    tab.
  - Agent-triggered: open in the background, hidden; never steal focus.
- Detail: General principle: agent-operated browser or emulator activity runs in
  the background and does not refocus or distract the human.
- Owner, verbatim:
  > Your recommendation about pop-ups makes sense. Could we keep that as like a
  > pop-up window and still keep the original tab alive? If not, a new tab is
  > fine, but I like the idea of a pop-up being a pop-up window that kind of
  > sits on top of our entire application. Oh, but only if it's human
  > controlled. If it's agent controlled and the agent is the one filling out
  > the auth or whatever, prefer to hide it away. Like, it's really distracting
  > when the agent keeps on refocusing on a browser to do things or an emulator
  > to do things while I'm trying to do things as a human. So if you can do
  > things in the background, then that's definitely a preference for agent
  > operated stuff.
- Reason: OAuth and payment pop-ups need `window.opener`; the human must not be
  interrupted by agent work.

---

[2026-10-13T20:45+1300@6bc650f] GitHub permalinks

- Requires: NO BLOCKERS (future idea; not now)
- Detail: A "GitHub permalinks" action: a link to the file on GitHub at the
  current commit, optionally with a line number. Zed has Open and Copy File
  Permalink; it would join the file context menu ("File and folder context
  menu") later. Name it "GitHub permalinks" in the UI so its meaning is clear.
- Owner, verbatim:
  > Oh, okay. About permalinks, we can write that down in the to do. We won't do
  > it now, but it's a good idea to add GitHub permalinks in the future. And
  > maybe we'll need to call it GitHub permalinks so that I actually know what
  > [expletive removed] it's talking about.
- Reason: owner asked for it recorded; shareable links to code.

---

[2026-10-14T00:02+1300@6007201] Markdown: embedded files (`![alt](path)`)

- Requires: NO BLOCKERS (reuses `crate::media`, the image decode and
  audio playback wrappers; sibling of "Markdown rich text — follow-ups")
- Detail: The Markdown renderer embeds files with the GitHub image syntax,
  `![alt](path)`: a link with a leading `!`.
  - Images render in place; video and audio are possible, through the media
    wrappers (decode/playback libraries) rather than new code.
  - Basic options for the rendered width and height; syntax to decide.
  - Non-media files (e.g. text) embed as a read-only, scrollable code block,
    collapsed to a file header; click the header to expand.
- Owner, verbatim:
  > the markdown renderer also needs to support embedded files, such as embedded
  > images, or maybe videos, or maybe audio. So that's a good use case to reuse
  > those things that we had earlier, maybe. [...] Maybe it's just exclamation
  > mark, square bracket, alternative text, closing square bracket, round
  > bracket, path to the file, closing round bracket. [...] maybe we need to
  > customize the width or the height of the file being rendered. I also think
  > maybe we don't have to restrict it to images [...] if it's a text file, maybe
  > it shows up in a code block. in full, but maybe it's scrollable. And read
  > only. Maybe it's got a thing you have to click to expand to show all of it,
  > and otherwise it's just like a file header.
- Reason: owner request; embedded media and files in Markdown.

---

[2026-10-14T00:02+1300@6007201] Markdown: text colour and text background colour

- Requires: a syntax decision (Markdown has none for colour)
- Detail: Colour for Markdown text: foreground colour and background colour.
  Syntax open.
- Owner, verbatim:
  > I'd also like to add some way to add coloring to the markdown, like text
  > color, [...] text background color.
- Reason: owner request.

---

[2026-10-14T00:02+1300@6007201] Indexer: Markdown free text and/or code symbols

- Requires: a design decision (scope)
- Detail: An indexer. Open: a free-text index for Markdown, a symbol index for
  code, or both.
- Owner, verbatim:
  > on the to-do list, we definitely need some kind of indexer. I'm not sure if
  > that would be like a free form text indexer for Markdown or if it would be
  > code only for symbols.
- Reason: owner request; search across files.

---

[2026-10-14T00:02+1300@6007201] Scroll-to-top and scroll-to-bottom icons

- Requires: NO BLOCKERS
- Detail: Small icons to jump to the top or bottom of a long terminal, file, or
  other scrollable view, shown when the content needs scrolling.
- Owner, verbatim:
  > We could perhaps add to the to-do adding little icons to scroll to the top or
  > the bottom of the terminal or a file, etc. if it's long or if you have to
  > scroll.
- Reason: owner request; long scrollback and files.

---

[2026-10-14T00:02+1300@6007201] Update CEF: a separate command

- Requires: NO BLOCKERS
- Detail: A command, not part of the normal build, that updates CEF (the
  Chromium Embedded Framework):
  - fetch the new CEF, build and test against it;
  - keep the old CEF until the new one builds, then remove the old one so old
    copies do not accumulate;
  - candidate for CI automation.
  Dependency updates stay manual; maybe an "updates available" report.
- Owner, verbatim:
  > our cargo build probably also needs like a separate command or something. Not
  > part of the normal cargo build, but some sort of command to update the
  > Chromium embedded browser and our dependencies. Or maybe not the
  > dependencies. Maybe we do that manually. Maybe it just tells us that there's
  > updates available. But updating Chromium embedded browser, that feels like
  > something that we should be doing fairly frequently, maybe even
  > automatically in the CI. [...] if it all still compiles against it, then we
  > can clean up the old version so we don't have them lying around on our
  > machine.
- Reason: CEF should stay current; check whether a new version breaks the build
  without leaving old copies behind.

---

[2026-10-16T21:23+1300@ad13ff26] Ribbon overflow menu

- Requires: NO BLOCKERS
- Detail: A three-dots button in the ribbon lists every ribbon option
  (soft-wrap, autosave, ...); a config option chooses which of them also show
  as ribbon icons.
- Reason: GitHub issue #1; keeps the ribbon usable when narrow.

---

[2026-10-16T21:23+1300@ad13ff26] Comment mode

- Requires: NO BLOCKERS
- Detail: Anchor comment threads to parts of a file (a line or range), shown
  beside the text. Uses: talking to agents about specific lines, and showing a
  PR's review threads.
- Reason: GitHub issue #1 (seen in the Claude Code app).
