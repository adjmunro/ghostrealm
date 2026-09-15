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
- Detail: Terminal mouse (selection, click-to-position, scroll/OSC mouse
  reporting) inside a pane; right-click context menu on vtabs (mark read/unread,
  dismiss, rename, close). Today mouse only switches vtab/pane/htab focus.
- Reason: expected terminal UX; context actions are in the brief.

---

[2026-09-15@1d4d22e]

- Requires: NO BLOCKERS
- Detail: Resize debounce during an active window/split drag: the per-frame resize
  now fires only on an actual (cols,rows) change, so intermediate sizes still
  reflow the PTY once per changed step; a short debounce would coalesce a drag into
  one reflow at the end.
- Reason: polish, none blocking.

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
- Detail: Double-tap Shift now toggles the palette (`tap::TapDetector`, keeping
  Cmd+K). Remaining: expose the trigger + the 300ms window in config (the chord
  model can't express a double-tap, so add e.g. `[input] double_tap_window_ms` and
  a way to bind double-tap-<mod> -> command id), rather than the current hardcoding.
- Reason: user's preferred muscle memory; keep it tunable/shareable.

---

[2026-09-15@1641f11] Sidebar context menu + new-tab button; mark-unread

- Requires: right-click menu widget (none yet)
- Detail: (1) Right-click a vtab → context menu: mark read/unread (show only the
  state-appropriate one — if read show "Mark unread", if unread show "Mark read"),
  Dismiss (when needs_input), rename, close, pin. (2) A "+" / new-vtab button at
  the very bottom of the sidebar. The `tab.mark_unread` and `tab.mark_read`
  registry commands both exist now; the menu picks which to show from state.
- Reason: brief's per-vtab context actions; discoverability.

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
