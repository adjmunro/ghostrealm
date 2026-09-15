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
