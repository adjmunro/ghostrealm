# Ideas.md

I'll dump things in here from time to time for you to discuss with me later.

---

If we get to vtab groups, we'll want air traffic control: we should be able to assign a directory to each vtab (also any new terminal sessions should start there as the root/cwd - we'll alos need a way to move the assigned root after the fact via the context menu / commands), then when you make a new vtab it should autogroup if it's in the same directory.

Do we re-render the whole screen when dirty? Or can we renderer only part of it?

---

Drag a htab to re-place it / create a split (design — needs more thought):
pick up a tab, and as you drag over a pane, show a drop hint that splits that pane
horizontally or vertically depending on where the cursor is (near which edge —
"further from centre" chooses the axis, and which half chooses the side). Dropping
splits the target pane and moves the surface there. Harder case: dropping to make a
new split that spans *existing* panes — e.g. you already have two side-by-side
vtabs/panes and want a new full-width pane the same height across the bottom, not
nested inside one of them. The binary split tree can't express that as a simple
leaf split; may need drop zones at the workspace edges (split the whole workspace)
vs pane-interior zones (split that leaf). Mull over the tree model implications.

---

Shell LSP assist (exploratory — not yet): help write good zsh while typing in the
terminal — completions/diagnostics from a shell language server (or shellcheck for
diagnostics). Open questions: how to hook into the live command line without a real
editor buffer (the shell owns the line), whether to intercept before the PTY or run
it against a scratch buffer, and how it interacts with the shell's own completion.
Likely pairs with the basic text-editor pane (TODO) where we *do* own the buffer.
