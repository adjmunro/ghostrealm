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
- Detail: Terminal mouse reporting misses the horizontal wheel, and alternate
  scroll (mode 1007: wheel -> arrow keys on the alternate screen when tracking
  is off) is not implemented.
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

[2026-09-15@1641f11] Sidebar context menu: Pin

- Requires: NO BLOCKERS
- Detail: The workspace menu has no Pin (the tree has no pin/ordering concept
  yet; how pins combine with inbox sections and manual order is undecided).
- Reason: brief's per-vtab context actions; discoverability.

---

> The entries below are from a live testing session (2026-09-15, @2e8e8c7). They
> capture observed bugs/regressions and requests; diagnoses are hypotheses written
> while the author still had the code context.

---

[2026-09-15@2e8e8c7] Scroll lag — pacing landed; deeper cost is the render itself (P1 follow-up)

- Requires: NO BLOCKERS (the VT worker thread landed in 92d5ca0)
- Detail: DONE (b173b08): wheel events now apply the scroll to the VT immediately but
  pace the redraw through the frame clock (frame_pending) instead of an immediate
  request_redraw per event, so a flick coalesces into ~one render/frame instead of a
  vsync-blocked backlog. REMAINING: each paced frame still re-snapshots the grid and
  reshapes newly-revealed rows on the UI thread (~fine in release, heavier in debug);
  the worker-thread architecture removes that from the UI thread entirely. Verify the
  feel after that lands; if still stuttery, profile the snapshot/reshape.
  Two worker follow-ups remain: rate-limit its grid builds under a sustained
  flood (it builds a grid per drained batch), and recycle the grids the UI has
  consumed instead of allocating new ones.
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

[2026-09-15@2e8e8c7] Naming — rename `vtab` → workspace in the code

- Requires: NO BLOCKERS
- Detail: The UI says workspace; the code still says `vtab` / `Vtab` /
  `VtabId`. Rename it to "workspace tab" where reasonable. Open: a scope
  prefix in palette labels (e.g. leading `[workspace]` / `[terminal tab]`).
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

[2026-09-15@2e8e8c7] Splits — close and split buttons

- Requires: NO BLOCKERS
- Detail: A close "x" on a pane and split right/left/up/down buttons. Split
  ratios are not saved between runs (nothing in the workspace tree is).
- Owner, verbatim:
  > Splits probably need an 'x' icon? And maybe also a split right/left/up/down
  > button(s)?
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
- Detail: Soft-wrap follow-ups. (1) A terminal toggle, if wanted
  (terminals already wrap to their column count, so a terminal toggle would
  need a ghostty reflow/no-wrap mode — likely skip). (2) Wrapped editors
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

[2026-09-29@19e0239] File browser — flat "recent" and "largest files" views

- Requires: NO BLOCKERS (extends `core::fs_tree::FsTree`, already headless)
- Detail: The grouped views (`FsView`: extension, kind, git status, modified,
  size) exist. Not yet: two flat lists across the tree, **recent**
  (newest-first) and **largest files**, with the same dim disambiguating
  prefix the grouped views show; and a **Staged** group in the git status view
  (it has Modified, Untracked and Unchanged).
- Reason: navigate by kind/status/date, not just by folder; requested + brainstorm.

---

[2026-10-05T20:35+1300@3f581979] Files surface — media follow-ups

- Requires: NO BLOCKERS
- Detail: Images and audio open as a Files surface's `MediaView`. Remaining:
  - **video**: decode + play (an ffmpeg-class dependency decision; spike
    separately). Reuse the audio controls and `PaintCx::image` per frame.
  - **autoplay toggle**: the `autoplay` button is a per-document override
    of `[file_browser] preview_autoplay`; write it back to the config instead,
    if that is the wanted behaviour.
  - **wake, not poll**: `MediaView` polls its loader on a 15 ms timer while a
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
- Detail: Markdown shows in three view modes (Preview, Integrated, Source;
  PLUGINS.md "View modes"). Remaining:
  - Preview: selecting and copying text; images and embeds (alt text shows
    for now); `#anchor` links within the document; a double click edits at
    the block's first line, not at the word clicked; table column alignment
    (`:-:`) is ignored; a quoted list indents its quote and list levels in
    one fixed order.
  - A Markdown feature checks whether a link/image target exists (a local file
    on disk; a reachable URL needs a network decision) and flags broken ones
    with inline error markers.
  - Code blocks: horizontal scroll inside a block (without soft wrap, a long
    line runs out of its box; in Preview always); for shell-flavoured
    blocks, a run button in the left gutter for each runnable command; an
    LSP for the fence's
    language; highlighting a fence in a stateful language (Markdown in
    Markdown) needs the inner state carried too; see "DISCUSSION: syntax
    highlighting direction".
  - Links: look and feel (colours) of internal and external links, to discuss.
  - Nesting: the highlighter gives each char one token, so emphasis inside
    strong (or a link or heading) loses the outer style; needs a token set per
    char, or style flags on spans.
  - Bold has no synthetic fallback: a monospace family without a bold face
    shows the nearest weight (italic is skewed by the shaper).
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
- Detail: Remaining: (1) move the rest of the app's own chrome (sidebar, tab strips, palette,
  menu) onto `PaintCx` as the empty-pane picker and dir picker already are,
  retiring their per-widget buffer pools; (2) extract the API
  into its own crate for out-of-tree Rust plugins — needs commands registered
  against a trait rather than `Registry<AppState>`; (3) per-plugin typed config — each
  plugin already documents its sections (`Plugin::config_sections`), but the typed
  fields still live on `core::Config`; a plugin should deserialize its own section; (4) view-scoped
  commands and keybindings (e.g. editor-only chords in `[keybindings]`, palette
  entries that act on the focused view); (6) dynamic loading / WASM for
  third-party plugins, if ever.
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
  Enter opens the file (in a Files surface, as every open does). Make the triggers
  configurable (the existing `[input] double_tap_window_ms` + remappable actions).
  Later: if the root is a code repo, index code SYMBOLS (via the LSP) and search
  those instead of/along files.
- Reuse: the popup's content search can run `core::search::Searcher` (the
  Files sidebar's `?` mode: worker, generation cancel, streamed `FileHits`)
  and draw its results as `file_browser::search` does.
- Reason: fast keyboard-only navigation; requested.

---

[2026-10-04@c994011] Floating picker — follow-ups

- Requires: NO BLOCKERS
- Detail: The picker chooses a directory, a file (`editor.open`), or where to
  save (Save As). In save mode typing goes to the name, so the list can't be
  filtered or given a typed path — e.g. Tab to move focus between the name and
  the header field.
- Reason: the picker is now the app's open/save dialog; it should feel like one.

---

[2026-10-13@13e2bda] Editor — table of contents follow-ups

- Requires: code symbols need the LSP (document symbols)
- Detail: The sticky table of contents lists Markdown headings and TOML tables
  (PLUGINS.md "Table of contents"). Not yet: code files' symbols once an LSP
  is wired (a second outline source beside the highlighter's); setext headings
  in Integrated and Source (the highlighter is line-local; Preview lists
  them). Titles can differ between the modes: Integrated and Source strip what
  the line-local highlighter marks (a link it doesn't recognise keeps its
  brackets), Preview takes pulldown-cmark's text (escapes resolved, reference
  links, HTML dropped). Undecided: whether the current section follows the
  caret rather than the top line; keyboard use (a command or chord that opens
  the panel, arrows and Enter in it); folding a heading's subsections in the
  panel.
- Reason: navigate long files (the config included) by structure.

---

[2026-10-13@13e2bda] Editor — inline git change highlights

- Requires: line-level diff hunks (`core::line_diff` only counts today)
- Detail: While the header's statistic shows changes (or via a toggle), tint line
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
  feed the same layer later. The editor never knows about git. Comment mode's
  thread markers share the gutter slot.
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
    findings), 24 (stuck mount), 36 (unbracketed paste),
    and the "To confirm" note in [stack-review-findings.md](stack-review-findings.md).
  - The open design items: the profile scope controls and picker (TODO "Web —
    profiles"), syntax highlighting direction.
  Decided in the review rounds and no longer open: Chromium sandbox on, web
  profile location, pop-up handling, shell exit, Delete = Trash, GitHub
  permalinks (future), the file-browser date view naming, the Files surface
  (GitHub issue #1).
- Reason: several decisions are pending and may interact.

---

> The entries below are from the project owner's review of build 914539d
> (2026-10-13). All are HIGH PRIORITY (quality of life). Each was checked
> against e9617b8; notes that were already fixed are not listed. The "Small
> fixes" entries are actionable now; the "Discussion" entries need a design
> session before any code.

---

[2026-10-14@7364079] File and folder context menu — remaining

- Requires: NO BLOCKERS (each item below is independent)
- Detail: The menu exists (right-click in a Files surface's sidebar or on a
  file tab, a document's file name in its header and its `⋯`; README,
  PLUGINS.md). Not built:
  - **Undo / Redo** of file operations (Zed's Edit section). Needs an operation
    log with an inverse for create, duplicate, paste, rename, move, and Trash
    (move back from the Trash). Left out rather than half-done.
  - **Finder interop for Cut / Copy / Paste**: the file clipboard is the app's
    own; the system pasteboard (file URLs) would let Paste work with Finder.
  - **Delete on other platforms**: only macOS moves to the Trash
    (`NSFileManager trashItemAtURL`); elsewhere it reports "not supported" and
    never deletes permanently. The `trash` crate or the freedesktop spec would
    cover Linux.
  - **Cmd+Backspace** on the file browser's selection to delete (from the old
    "Delete files" entry).
  - New File / New Folder create the entry and do not open it.
- Not in the menu: **GitHub permalinks** (see "GitHub permalinks"), Open
  Markdown Preview (the document header's view-mode button shows it).
- Reason: the first menu covers the owner's list; these are the edges.

---

[2026-10-13T20:25+1300@e9617b8] Files surface — remaining from GitHub issue #1

- Requires: NO BLOCKERS
- Detail: The Files surface (DECISION.md) is built. Not yet: dragging file
  tabs to reorder them, and the panel controls at the tab row's right end
  (search, `⋯`, maximise, close; issue screenshot img25). Pane close and
  split buttons: "Splits — close and split buttons".
- Reason: owner review; clone the Claude Code app's file panel, plus what
  the issue says the owner likes.
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

[2026-10-23@b179105] Web — verify the Chromium sandbox on Linux

- Requires: a Linux machine
- Detail: The host leaves Chromium's sandbox on (DECISION.md, Web surface /
  security), so on Linux Chromium needs unprivileged user namespaces or a
  setuid-root `chrome-sandbox` in the bundle directory; without either it
  refuses to start. The bundler copies `chrome-sandbox` but does not make it
  setuid. Check that the host starts and pages load (`cargo test -p
  ghostrealm-web --test host`), and decide how the setuid helper is installed
  where user namespaces are restricted (e.g. Ubuntu's AppArmor policy).
- Detail: When the sandbox is unusable the host dies before Ready without a
  `Failed` event; the reason reaches only `host.log` and stderr, which the
  client discards. Surface it in the pane.
- Reason: only the macOS sandbox has been verified.

---

[2026-10-13T20:45+1300@6bc650f] Web — profiles: picker, human-only vs agent, per-workspace scope

- Requires: a design session for the picker and the scope controls; the agent
  channel → running app entry for the shared host
- Detail: Decided: the profile lives in Application Support, not
  `~/Library/Caches/ghostrealm/web` (DECISION.md, Web surface / profiles).
- Detail: The host's root holds one profile today, Chromium's `Default`: CEF
  154 puts the global request context there whatever `CefSettings.cache_path`
  says. Other profiles have to be `CefRequestContext`s with a `cache_path` of
  their own in the same root (untried).
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
  code, or both. `core::search::Searcher` (the sidebar's `?` search) walks
  and reads every file per query; an index would answer it without the walk,
  behind the same `SearchQuery` / `FileHits` interface.
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

[2026-10-16T21:23+1300@ad13ff26] Comment mode

- Requires: where threads are stored (decision pending: local sidecar, in
  the repo, GitHub only, or local plus PR threads)
- Detail: Anchor comment threads to parts of a file (a line or range), shown
  beside the text. Uses: talking to agents about specific lines, and showing a
  PR's review threads.
- Hooks the editor already has or plans:
  - A thread draws as a `RowKind::Widget { after_ln, id }` row in the
    editor's `RowTable` (PLUGINS.md "Syntax highlighting"), its height its
    own: every y-to-text mapping already reads the table, so clicks and the
    caret skip it. Nothing places one yet.
  - A gutter decoration slot (a marker per line, with hover and click) shared
    with "Editor — inline git change highlights": both are providers to the
    same decoration layer.
  - A `CommentStore` keyed by path + line anchor + a hash of the anchored
    lines' content, so a thread finds its lines again after edits (and shows
    as outdated when they are gone).
  - `comments.*` agent commands (list, add, reply, resolve), within the
    agent channel's access scope.
  - PR review threads read and written through `gh` (its auth, no token of
    our own), mapped onto the same anchors.
- Reason: GitHub issue #1 (seen in the Claude Code app).

---

[2026-10-18T05:18+1300@e3a7c9e] View modes for other languages

- Requires: Kotlin highlighting or a language server (document symbols)
- Detail: Integrated sizes a row by `Highlighter::heading_level`, and Preview
  exists for Markdown only. Per language:
  - Integrated formatting: give a language headings through its highlighter's
    `heading_level`, or from LSP document symbols where a server runs. Kotlin:
    top-level classes as H1, shrinking with nesting; functions and non-local
    properties as H3/H5. The same levels feed the outline and table of
    contents.
  - A code Preview: render doc comments (KDoc, rustdoc) as Markdown between
    the declarations, plus generated diagrams (class relationships, method
    flow). It would be a `ViewMode::Preview` for that language, read-only
    like Markdown's.
- Reason: GitHub issue #1.

---

[2026-10-23@290d752] Web — opt-in main-world eval

- Requires: owner decision
- Detail: Decision needed: offer an explicit opt-in eval in the page's own
  JavaScript world, e.g. for reading a localhost dev server's app state while
  debugging? `web.eval` runs isolated (DECISION.md, Web surface / security), so
  it can't see page globals. A main-world result is page-controlled, so it
  would have to be requested per call and marked as such.
- Reason: isolation is the safe default, but app-state debugging needs page
  globals.

---

[2026-10-23@e32ebd4] Web — JS dialogs hang the page

- Requires: an owner decision on how web downloads, uploads, and JS dialogs
  are handled
- Detail: The host has no `JsDialogHandler`, so a page calling `alert()`,
  `confirm()` or `prompt()` waits forever for an answer nothing gives: evals
  and navigations on it stall. How dialogs are answered (shown to a human,
  auto-dismissed and reported to an agent) waits on that decision.
- Reason: a page must not be able to freeze its pane or the agent driving it.

---

[2026-10-23T09:43+1300@5a747143] Web — check floating pop-ups on a display

- Requires: a human on a desktop session
- Detail: A pop-up window a person opens floats over its opener's page
  (DECISION.md, Web surface / pop-ups). Tests cover its routing, focus,
  and closing headlessly; check how it looks (border, shadow, header, close
  button hover and its tooltip) and that clicks, typing, and scrolling in
  it reach it. Also check a person's `target=_blank` tab opens beside its
  opener.
- Detail: Kill the web host (`kill -9` its pid) with a floating pop-up, a
  tab a page opened, and an `about:blank` pop-up open: the first two come
  back at their address once the host restarts (the floating one still
  floating with the keyboard), and the third leaves a warning toast on its
  opener's pane.
- Reason: no display in the agent sandbox.

---

[2026-10-23T09:43+1300@5a747143] Web — float pop-ups over the whole window

- Requires: NO BLOCKERS
- Detail: A floating pop-up is bounded by its opener's page. Over the
  whole window, the window must route the pointer (presses, hover, wheel)
  to a view outside its pane while it is over the pop-up, and keep chrome
  buttons, dividers, and tab strips under it from taking them.
- Reason: the owner pictured a pop-up over the entire app.

---

[2026-10-23T09:43+1300@5a747143] Web — stacked floating pop-ups

- Requires: NO BLOCKERS
- Detail: Of several pop-up windows floating over one page, only the
  newest shows and takes input; the others wait under it until it closes.
  A pop-up a floating pop-up opens floats inside it, and draws its shadow,
  border, and header under the outer pop-up's page image (overlay quads
  draw before overlay images).
- Reason: OAuth flows rarely stack pop-ups; showing them all needs a way to
  switch between them and a layer per pop-up.

---

[2026-10-23T18:52+1300@cfbb8bc1] Web — revisit the person/agent pop-up rule if agents drive the GUI

- Requires: the agent channel moving into the running app (agent.rs; TODO.md
  "Agent channel → the running app")
- Detail: A pop-up is a person's when a press or key reached its opener in
  the last 5 s (DECISION.md, Web surface / pop-ups). Today agents drive only
  the headless `ghostrealm agent`, where no person input arrives. Once they
  drive the GUI, a gesture eval within 5 s of a person's click would count
  as the person's: a visible, focused tab that the pop-up cap doesn't count.
  Tell the two apart by the source of the activation, not by timing.
- Detail: Likewise a page is an agent's when opened without a window (the
  agent channel): the host keeps an agent's page visible to Chromium off
  screen. In the GUI every page `web.new` opens is a person's, an agent's
  included; tell them apart by the command's source.
- Reason: agent activity must stay in the background.

---

[2026-10-23T22:46+1300@0fd87271] Web — a host test's host can fail to start

- Requires: NO BLOCKERS
- Detail: Under heavy load a host in `crates/web/tests/host.rs` now and then
  dies with "host never became ready: failure None, stdout closed true,
  status ... (9)": SIGKILLed while starting, before it creates its profile
  dir (none is left behind), so before Chromium's ProcessSingleton runs.
  Seen 3 times in about 40 suite runs on 2026-10-31, all with the
  machine's load average over 40 and swap all but full (10.3 of 11.2 GB
  used) on an 8 GB machine. The system log shows no jetsam or memorystatus
  kill for the host. The suite now runs at most four hosts at once
  (`HOSTS_AT_ONCE`), which made it rarer. Find the sender (`dtrace` on
  `proc:::signal-send`, or `log stream` while it happens). Run with
  `GHOSTREALM_WEB_DEBUG=1` to see its stderr.
- Reason: a flaky test hides real failures.

---

[2026-10-24T02:09+1300@08234dd1] Web — modified clicks: frames, posting forms, hidden links

- Requires: NO BLOCKERS
- Detail: A link in a frame clicked with Shift, Cmd/Ctrl, or the middle
  button opens nothing: the host reopens these links through
  `window.open` in an eval, and evals reach only the main frame (DECISION.md,
  Web surface / pop-ups, 2026-10-24T02:09). Run the eval in the link's own
  frame, so its sandbox flags apply (CEF's frame id is not known to match
  the DevTools one the eval needs).
- Detail: A link the eval can't find (in a closed shadow root, removed)
  and a form submitted so open without a referrer.
- Detail: A form that posts, submitted with a modifier, is submitted again
  in place (its submit handlers run twice) instead of posting in a new tab,
  and one submitted with the focus elsewhere (by script) opens its URL in a
  new tab as a GET, without its data. A disabled submit button (a guard
  against a second submit) submits nothing. After Enter, a page's own
  `form.submit()` (Cmd+Enter in an editor, say) is taken for an implicit
  submission: the form is submitted with its first submit button, adding
  that button's name and value and running the validation and submit
  handlers `submit()` skips.
- Reason: browsers open all of these in a new tab, with the referrer the
  link allows.

---

[2026-10-24T03:27+1300@eee9ed39] Web — guard the respawn after a host that failed while still alive

- Requires: a design
- Detail: `HostSlot::notice_exit` drops a host that failed but is still running
  (a `failed` event, or a protocol mismatch) through `failed()`, with no
  `Exiting` guard. The respawn follows the back-off (1 s), or for a mismatch
  the next Reload, `web.new` or bundle change, which can come at once. If the
  process is still exiting then, the respawn can find the profile still locked:
  it reports the profile in use and backs off again.
- Detail: Setting `exiting` there needs a design. `retry_at()` would return the
  reap time and ignore the back-off; GUI `start()` would report a failure as a
  restart; headless `host()` would block in the exit wait during the back-off.
- Reason: a failure shouldn't cost a second, spurious "profile in use" failure.

---

[2026-10-26T22:50+1300@5bb2c09d] Toasts — the clock may run under a pointer already over the window at launch

- Requires: a desktop check
- Detail: `pointer_in` (`window.rs`) starts false, so a launch-time toast
  under a pointer that is already over the window runs its clock until the
  first `CursorEntered` or `CursorMoved`. Check whether winit sends
  `CursorEntered` at launch in that case; if not, start from the pointer's
  real position.
- Reason: a toast under the pointer should wait to be read.

---

[2026-10-26T22:54+1300@fdc79626] Palette — a prompt error row may hide its last wide glyph

- Requires: a desktop check with a CJK or emoji error message
- Detail: The argument prompt's error rows are drawn in `line_h`-tall
  buffers `text_w` wide with `Wrap::WordOrGlyph` (`build_palette` in
  `crates/app/src/window.rs`). `wrap_cols` fits rows by cell count, a wide
  glyph counting two. If a fallback font draws CJK or emoji glyphs wider
  than two monospace cells, a row that fits by count overflows `text_w`
  and its last glyph wraps onto a second line the buffer's height hides.
  Check with a long CJK/emoji error; if it clips, draw the rows with
  `Wrap::None` or measure them by shaped width.
- Reason: the hidden glyph is dropped without an ellipsis.

---

[2026-10-26T23:00+1300@a49a439a] Text wrapping — `clusters()` splits flags and skin tones

- Requires: a new dependency (`unicode-segmentation`), which the owner's
  dependency policy discourages
- Detail: `clusters()` in `crates/app/src/window.rs` groups a base
  character with the zero-width characters after it and what a ZWJ joins.
  Regional-indicator pairs (flags such as 🇳🇿) and skin-tone modifiers
  (👍🏽) count as separate clusters, so `wrap_lines` and `wrap_cols` can
  break a too-long word, or cut for the ellipsis, inside one. Proper
  grapheme segmentation fixes it.
- Reason: a split flag or skin tone draws as two wrong glyphs.

---

[2026-10-30T08:15+1300@19b9a94a] clean-all — in-use detection on Linux

- Requires: a Linux machine to test on
- Detail: `scripts/clean-stale.rs` reads what runs only on macOS; elsewhere
  `Running::now()` is `None`, so `--all` empties no target dir and removes
  nothing from the temp dir. Reading `/proc/<pid>/exe` and
  `/proc/<pid>/status` (`Uid:`) would do, but only in the initial pid
  namespace: bwrap, Docker and podman mount a procfs of their own namespace,
  whose `NSpid:` has one field. Check `/proc/self/ns/pid`'s inode against
  the initial namespace's (`0xEFFFFFFC`) instead, and test it there.
- Reason: on Linux clean-all frees only superseded units.

---

[2026-10-31T09:22+1300@7ec72b2f] untested exit-waiter failure path

- Requires: NO BLOCKERS
- Detail: No test drives the `spawn_exit_waiter` failure path in
  `ThreadedTerminal::spawn` (`crates/terminal-ghostty/src/threaded.rs`),
  which kills and reaps the child under the `ChildState` lock. Add a seam
  that makes the thread spawn fail, and test that the child ends reaped.
- Reason: the path runs only when a thread spawn fails, so a regression in
  it would go unnoticed.

---

[2026-10-31T14:25+1300@b28f80e9] Web — pop-ups made again: review leftovers

- Requires: NO BLOCKERS
- Detail: DECISION.md (Web surface / pop-ups, 2026-10-31) gives "a new
  bundle" as a cause of a host restart, but nothing restarts a running host
  when the bundle changes: `bundle_stamp` only lifts a protocol-mismatch
  refusal. Drop it; a host that reports `failed` and is dropped
  (`HostSlot::notice_exit`) is a second cause.
- Detail: Orphan tabs are made with scale 1.0 (`OrphanTabs::take`,
  `for_popup(.., 1.0)`), but their `popup_size` is physical pixels at the
  opener's scale: an unpainted orphan tab, and pop-ups it opens, come back
  at the wrong CSS size after a restart. Carry the scale in `PopupInfo` and
  `WebView::release`, or use the slot's last known scale.
- Detail: The `DROPPED` cleanup in `popup_aborted` (crates/web-host
  browser.rs) has no test, nor has a pop-up `close_popups` drops before
  CEF makes it: in tests CEF always made the pop-up before its opener's
  `on_before_close`. Testing either needs a host test hook.
- Detail: A hidden pop-up can be named in two lost toasts
  (`AppState::tell_lost`): H made again with lost opener R, F floating over
  H also lost, F opened hidden P; P is found through H's surface and through
  F's page. Skip pop-ups already in `went_with` when building later roots'
  `with`.
- Reason: non-blocking findings of the review of c11a355..b28f80e.

---

[2026-10-31T15:27+1300@00d7e28d] Linux: build and place the web host bundle

- Requires: NO BLOCKERS (a Linux machine to test on)
- Detail: The app's build script (`crates/app/build.rs`) builds and places the
  web host only for macOS targets; nothing bundles it on Linux now that
  `mise run web` is gone. By hand:
  `cargo build -p ghostrealm-web-host --features bundle --bins --target-dir
  target/web-host --config 'build.build-dir="target/web-host"'`, then
  `target/web-host/debug/ghostrealm-web-bundle`, then copy the
  `ghostrealm-web-host/` dir it makes beside `ghostrealm` (`target/debug/`).
- Detail: To do: build and place the Linux bundle (a plain directory with the
  binary and CEF libraries) from `build.rs`, with an ownership guard of its own
  (the macOS one reads the bundle id from `Info.plist`, which the Linux bundle
  lacks), its completeness rules, and tests per guard.
- Reason: the Linux bundle has another layout and this machine can't run it,
  so the build script skips every OS but macOS.

---
