# Decision.md

## Instructions

- This files is a living document (managed by YOU) of important, high-level decisions (e.g. architecture, libraries chosen etc) that affect the project.
- Append to the tail of `## Decision Registry`.
- You can use `~` around the header to ~revoke~ decisions when they are redundant or no longer relevant.

## Format

- Record the datetime and current SHA at time of writing as context for posterity.
  > *Explicitly formatted as
  `[<datetime>@<commit-sha:0..8>]` with the square brackets - the header is not an optional field.
- Each element can have multiple bullet points for each category, decision detail, and reason.

```markdown
---

[<datetime>@<commit-sha:0..8>]

- Category: <scope>
- [Status: REVOKED: <why> | SUPERSEDED BY <[<datetime>@<commit-sha:0..8>]>]
- Detail: <what-was-decided>
- Reason: <why>
```

---

## Decision Registry

---

[2026-09-14T20:12+1200@e96f61f0]

> Baseline: written before version control; dated to the project's first commit
> (`e96f61f chore(repo): initialise project baseline`).

- Category: Terminal backend / core dependency
- Detail: Build on `libghostty-vt` (VT engine only) + own PTY (`portable-pty`) + own wgpu glyph renderer + font/shaping stack (`cosmic-text` candidate). `libghostty-vt` gives cells; we draw pixels. Ghostling (single-file reference terminal shipped with the crate) is the renderer starting point.
- Detail: Terminal substrate walled behind a trait seam from the app core; PTY / VT / renderer / fonts each behind their own internal seam so any one can be swapped as upstream Ghostty publishes more.
- Detail: Terminals render into the same wgpu scene as chrome (no native subviews) → unified compositing, palette overlay is trivial, fully cross-platform.
- Reason: Only path that reaches the cross-platform (macOS + Linux) goal. `libghostty-vt` is documented, ABI-managed, and has ready Rust crates. `libghostty-internal` (the black-box embedder) is macOS/iOS-only, undocumented, and pre-stable — see [libghostty-findings.md](libghostty-findings.md). We accept owning the renderer because Linux is a launch goal, not a "revisit later".

---

[2026-09-30T12:52+1300@7d132a83]

- Category: Architecture / content plugins
- Detail: Every surface's content is a `View` opened by a registered `Plugin` (`crates/app/src/plugin/`). Terminal, editor, and file browser are first-party plugins on the same API (`crates/app/src/plugins/`). Contract in [PLUGINS.md](PLUGINS.md).
- Detail: In-process Rust trait objects, immediate-mode paint through `PaintCx` into one shared `Frame` (quads + text via the app's single `FontSystem`/row cache/glyph atlas). No per-plugin GPU surfaces.
- Detail: Views live in `AppState` (headless), so the agent channel and tests drive them without a window. Effects outside a view go through a request queue the app applies after the call (views can't mutate the tree while borrowed from it).
- Detail: Input order: overlays → app chords (Cmd+, and `[keybindings]`) → focused view. Unhandled keys are dropped, never forwarded to another view.
- Detail: Named `Plugin`/`View`, not "panel": the glossary already uses Panel for a pane.
- Detail: The API stays inside the app crate for now; `Plugin::register_commands` takes `Registry<AppState>`, which would be a cycle in a separate crate.
- Reason: build new kinds (Git GUI, web browser, previews) as uniform modules that obey the same perf and input rules as the terminal. It also shrinks window.rs to layout/chrome/routing, and makes content testable headless.
