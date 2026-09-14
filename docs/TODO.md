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
- Detail: Perf/correctness follow-ups: (1) DECISION.md `@no-vcs` -> baseline SHA;
  (2) reuse RenderState/iterators across snapshots in the backend; (3) scrollback
  viewport (mouse wheel -> scroll_viewport); (4) resize debounce.
- Reason: polish, none blocking.
