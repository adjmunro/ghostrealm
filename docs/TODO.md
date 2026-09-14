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

[2026-09-14@806598c]

- Requires: NO BLOCKERS (keystone; unblocks palette/sidebar/agent)
- Detail: Integrate core into the app. Build an `AppState { tree: Tree, registry:
  Registry<AppState>, surfaces: HashMap<SurfaceId, GhosttyTerminal> }`. Render the
  active vtab's pane layout (recurse `Node`, lay panes out by axis/ratio), route
  input to the focused pane's active surface, and register real commands
  (tab.new, tab.close, split.leftright, split.topbottom, surface.new, focus.*).
- Reason: the window is currently a single standalone terminal; this turns it into
  the actual multiplexer and is the shared substrate the remaining features need.

---

[2026-09-14@806598c]

- Requires: AppState integration (above)
- Detail: Agent control/introspection channel (Phase 3, prioritised). A local
  channel (start simple: a control socket / line-delimited JSON) exposing the thin
  set `search(query)`, `get(...)`, `set(id,args)` over the registry, plus a
  text rendering of tree + grid state so an agent can observe without a screenshot.
  Scope an agent to its own vtab + app-wide settings.
- Reason: user-prioritised; also the dev/verification lever for driving the app
  headlessly (the sandbox cannot screenshot the GUI).

---

[2026-09-14@806598c]

- Requires: AppState integration
- Detail: Command palette overlay in the window — a floating layer above the wgpu
  scene, fuzzy search via `Registry::search`, Enter runs `execute`.
- Reason: Phase 2's third consumer of the registry; primary discoverability UI.

---

[2026-09-14@806598c]

- Requires: AppState integration
- Detail: Vertical sidebar (vtabs, inbox/read sections) + per-pane horizontal tab
  strips + focus routing + `autohide_single_tab`. Then the inbox status machine
  (Enter-optimistic busy, PTY foreground-pgid busy/idle, OSC 133 exit, hoisting)
  and settings TOML + Ghostty palette inheritance.
- Reason: Phases 4–6.

---

[2026-09-14@806598c]

- Requires: NO BLOCKERS
- Detail: Small follow-ups: (1) DECISION.md's `@no-vcs` marker → the baseline SHA;
  (2) reuse `RenderState`/iterators across frames in the backend instead of
  recreating them each `snapshot()` (~1ms/call, only paid on changed frames now).
- Reason: correctness/perf/polish, none blocking.
- Done: event-loop now wakes on PTY output via an `EventLoopProxy` + `Wait`, and
  only reshapes changed rows on changed frames (fixed the input lag).
