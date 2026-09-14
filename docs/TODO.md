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
- Detail: Busy detection for the inbox. Enter-submission = optimistic busy;
  authoritative busy/idle from the PTY foreground process-group; exit code via
  OSC 133 when shell integration is present. Then the auto-read dwell timer
  (`inbox.auto_read_after`) with the unfocus grace (`auto_unread_before`).
- Reason: completes Phase 5; makes the sidebar `busy` dot real (currently only
  read/unread/needs_input are driven).

---

[2026-09-15@1d4d22e]

- Requires: NO BLOCKERS
- Detail: Config-driven keybindings ([keybindings] map chord -> command id) and
  sidebar `side = "right"` layout. Chords currently hardcoded (Cmd+T/D/W/]/N/K).
- Reason: Phase 6 remainder; the config fields exist, wiring does not.

---

[2026-09-15@1d4d22e]

- Requires: reading Ghostty's resolved palette
- Detail: Chrome colour inheritance from the user's Ghostty config when [chrome]
  is unspecified (parse ~/.config/ghostty or read its palette). libghostty-vt does
  not expose this, so we parse it ourselves.
- Reason: brief's "chrome inherits the resolved Ghostty palette by default".

---

[2026-09-15@1d4d22e]

- Requires: NO BLOCKERS
- Detail: Terminal mouse (selection, click-to-position, scroll/OSC mouse
  reporting) inside a pane; right-click context menu on vtabs (mark read/unread,
  dismiss, rename, close). Today mouse only switches vtab/pane/htab focus.
- Reason: expected terminal UX; context actions are in the brief.

---

[2026-09-15@1d4d22e]

- Requires: NO BLOCKERS
- Detail: Small follow-ups: (1) DECISION.md `@no-vcs` -> baseline SHA;
  (2) resize debounce. (Scrollback and RenderState reuse now have their own
  dedicated entries below.)
- Reason: polish, none blocking.

---

[2026-09-15@1641f11] CRITICAL — responsiveness: the UI blocks on terminal work

- Requires: NO BLOCKERS
- Detail: While a command runs / emits heavy output in one surface, the whole app
  is stuck — you cannot switch vtabs/panes/htabs until it finishes and prints.
  Repro: a heavy `eza --tree --level=3` (user aliases it to `3` in zsh) lags for
  a while and pins the UI. Root cause (hypothesis): `render()` runs pump + snapshot
  + reshape on the UI thread; sustained PTY output floods the reader→channel, each
  wake requests a redraw, and a scrolling full screen reshapes every row (~26ms/
  frame, measured) at ~vsync, saturating the thread so winit input/click events
  queue behind it.
- Approaches: (a) rate-limit PTY-driven redraws (coalesce: drain all channel data,
  redraw at most ~60fps, and cap consecutive heavy frames so input is serviced);
  (b) budget reshape work per frame; (c) prioritise input — handle key/mouse even
  when output is flooding; (d) consider a steady frame clock (ScheduleWakeup-style)
  decoupled from the per-chunk waker; (e) profile with the heavy-output case.
- Reason: switching tabs while a command runs is table-stakes for a multiplexer.

---

[2026-09-15@1641f11] Switching vtab / split / htab is laggy (not instant on click)

- Requires: NO BLOCKERS
- Detail: Clicking a vtab, focusing a different split, or switching a htab is
  perceptibly slow. Likely: on any layout change `render()` resizes every pane's
  terminal and does `prev_row_hash.clear()` → a full-screen reshape of the newly
  shown surfaces on the switch frame; the shared row-buffer pool means switching
  away discards shaped text, so switching back re-shapes from scratch.
- Approaches: cache last (cols,rows,cell px) per surface and skip resize when
  unchanged; keep shaped text per surface (per-surface buffer sets, or an LRU) so
  re-showing a surface is instant; don't clear the whole row cache on focus-only
  changes; reuse RenderState/iterators across snapshots.
- Reason: switching should feel instant.

---

[2026-09-15@1641f11] Scrollback: mouse wheel + keyboard don't scroll

- Requires: NO BLOCKERS
- Detail: Neither the mouse wheel nor keyboard (Shift+PageUp/Down, Shift+Home/End)
  scrolls the terminal viewport. Wire winit MouseWheel + those keys to the vt
  `Terminal::scroll_viewport` and render the scrolled viewport; reset to bottom on
  new input/output per usual terminal behaviour.
- Reason: basic terminal function, currently missing.

---

[2026-09-15@1641f11] Command palette UX

- Requires: NO BLOCKERS
- Detail: (1) Click a palette result with the mouse to select/run it (hover to
  highlight); currently only ↑/↓ + Enter work. (2) Clicking outside the palette
  panel (anywhere on the dimmed backdrop) closes it. Both need hit-testing the
  palette panel/rows in on_click while `self.palette` is Some.
- Reason: expected palette interaction.

---

[2026-09-15@1641f11] Palette toggle = double-Shift (IntelliJ "Search Everywhere")

- Requires: NO BLOCKERS
- Detail: Change the default palette trigger to a double-tap of Shift (as in
  Android Studio / IntelliJ), keeping it configurable. Needs double-tap detection
  (two Shift presses within a short window with no other key between), which the
  chord/keybinding model doesn't express today — add a small tap-detector in the
  window; keep Cmd+K as an alternate default or via [keybindings].
- Reason: user's preferred muscle memory.

---

[2026-09-15@1641f11] Sidebar context menu + new-tab button; mark-unread

- Requires: right-click menu widget (none yet)
- Detail: (1) Right-click a vtab → context menu: mark read/unread (show only the
  state-appropriate one — if read show "Mark unread", if unread show "Mark read"),
  Dismiss (when needs_input), rename, close, pin. (2) A "+" / new-vtab button at
  the very bottom of the sidebar. (3) Add a `tab.mark_unread` registry command
  (only `tab.mark_read` exists); the menu picks which to show from current state.
- Reason: brief's per-vtab context actions; discoverability.

---

[2026-09-15@1641f11] Focus follows mouse in splits (configurable)

- Requires: NO BLOCKERS
- Detail: Hovering a pane focuses it (route keyboard there), as an option — add a
  config flag (e.g. [input] focus_follows_mouse) and, when on, update focused_pane
  on CursorMoved hit-test. Decide the default.
- Reason: requested; common tiling-WM behaviour.

---

[2026-09-15@af1c592] "Run Anything" — double-Ctrl (IntelliJ style)

- Requires: double-tap detector (shared with the double-Shift palette trigger)
- Detail: Double-tap Ctrl opens a single-line input; on Enter, open a NEW vtab and
  run the typed command in the user's default shell (like IntelliJ "Run Anything").
  Reuse the palette overlay's input rendering; distinguish mode (command vs command-
  palette). Configurable trigger via [keybindings].
- Reason: fast "spawn a tab running X" flow.

---

[2026-09-15@af1c592] Basic text-editor pane (non-terminal surface type)

- Requires: perf work first (per user); generalising a pane's content beyond a
  terminal surface
- Detail: A minimal editable text frame as an alternative pane/surface content, so
  panes aren't only terminals. Needs the model to allow a surface to be a terminal
  OR an editor buffer, plus text input/editing + rendering (reuse the glyphon text
  path). Keep scope minimal to start (open/edit/save a file). May later host the
  shell-LSP idea (see IDEAS.md).
- Reason: requested; expands the app beyond terminals.
