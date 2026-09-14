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

[2026-09-14T20:12+1200@no-vcs]

> `@no-vcs`: repository not yet under version control at time of writing; replace with SHA once initialised.

- Category: Terminal backend / core dependency
- Detail: Build on `libghostty-vt` (VT engine only) + own PTY (`portable-pty`) + own wgpu glyph renderer + font/shaping stack (`cosmic-text` candidate). `libghostty-vt` gives cells; we draw pixels. Ghostling (single-file reference terminal shipped with the crate) is the renderer starting point.
- Detail: Terminal substrate walled behind a trait seam from the app core; PTY / VT / renderer / fonts each behind their own internal seam so any one can be swapped as upstream Ghostty publishes more.
- Detail: Terminals render into the same wgpu scene as chrome (no native subviews) → unified compositing, palette overlay is trivial, fully cross-platform.
- Reason: Only path that reaches the cross-platform (macOS + Linux) goal. `libghostty-vt` is documented, ABI-managed, and has ready Rust crates. `libghostty-internal` (the black-box embedder) is macOS/iOS-only, undocumented, and pre-stable — see [libghostty-findings.md](libghostty-findings.md). We accept owning the renderer because Linux is a launch goal, not a "revisit later".
