# Glossary.md

Mutual project lexicon so we both know what we're talking about. Aim to be consistent with this document. Add only terms you don't understand / have found ambiguous and needed explaining, with names discussed with your human before appending.

---

- Workspace: a named, status-bearing entry in the sidebar; selecting it shows its panes in the main area. Each workspace owns its own recursive split tree of panes and (planned) a pinned root directory. This is the user-facing name; the code still calls it a vertical tab / `vtab` / `Vtab` / `VtabId` (an internal rename to "workspace tab" is a tracked follow-up).
- Sidebar: the vertical list where the workspaces live.
- Terminal tab (horizontal tab / htab): a tab in a pane's top strip, belonging to one terminal (or editor) surface in that pane; shown only when a pane holds more than one.
- Panel / Pane: a single terminal-or-editor frame within a workspace. One workspace may be geometrically split into multiple panes horizontally or vertically.
- Command palette: the popup window that allows the user to search and run command actions from the registry.
- Command registry: the place where all "actions" an "actor" (user->UI button, user->palette, agent->MCP) can take live for lookup and execution.
