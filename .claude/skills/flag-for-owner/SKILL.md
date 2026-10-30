---
name: flag-for-owner
description: Use when work hits a decision only the owner can make, an irreversible or wide-blast-radius action, a problem you can't solve, or a check that needs a human at a desktop.
---

Record it in `decisions-pending.md`, skip it, carry on with other work, and raise it when the owner is present. Never guess on an irreversible or wide-reaching decision.

Where it goes in `decisions-pending.md`:

- **Owner's decision:** a new numbered section, plus its line in the list at the top, in the existing shape: **Source**, **What's being decided**, **Background**, **Options**, **Recommendation**, and an empty **Your answer:** `>` block.
- **Small and reversible:** take the default yourself and add a line under "Decisions I'd take by default unless you object".
- **Needs a human at a desktop:** a line under "Needs you at a desktop".
- **Unsolved problem:** a numbered section with what you tried and what blocked you, in place of **Options**.

Before building on an item, check whether its **Your answer** block is filled in. Whoever writes the entry commits and pushes it on their own branch: a worker on its goal branch; the director on a small branch from `develop`, never a worker's branch. It reaches `develop` through the same review as any other commit, so name the branch when raising the item. The director's branch doesn't land while a reviewed goal branch waits to fast-forward: after the goal branch lands, a 🔨 Code Implementer rebases the director's branch onto `develop` (`docs/GIT.md` says how without force-pushing). Read-only roles put the flag in their report and the director records it.
