# Glossary.md

Mutual project lexicon so we both know what we're talking about. Aim to be consistent with this document. Add only terms you don't understand / have found ambiguous and needed explaining, with names discussed with your human before appending.

---

- Workspace: a named, status-bearing entry in the sidebar; selecting it shows its panes in the main area. Each workspace owns its own recursive split tree of panes and (planned) a pinned root directory. This is the user-facing name; the code still calls it a vertical tab / `vtab` / `Vtab` / `VtabId` (an internal rename to "workspace tab" is a tracked follow-up).
- Sidebar: the vertical list where the workspaces live.
- Terminal tab (horizontal tab / htab): a tab in a pane's top strip, belonging to one terminal (or editor) surface in that pane; shown only when a pane holds more than one.
- Panel / Pane: a single terminal-or-editor frame within a workspace. One workspace may be geometrically split into multiple panes horizontally or vertically.
- Command palette: the popup window that allows the user to search and run command actions from the registry.
- Command registry: the place where all "actions" an "actor" (user->UI button, user->palette, agent->MCP) can take live for lookup and execution.
- Document: one file a surface shows (`DocId`). Most views are their own single document; a view can host several (one per file tab). Saving, closing (Cmd+W), and the unsaved-changes questions act per document.
- Files surface: the surface (plugin `files`) every opened file shows in: a file sidebar, its documents, and a slim top row with the sidebar button and a file tab per document.
- File tab: a document's pill in a Files surface's top row (file icon, name, close slot). Not a pane tab (htab): those hold surfaces. An unsaved one shows a dot in its close slot, × while hovered; pane tabs show the same dot.
- Preview tab: the one file tab a single click in the sidebar opens, its name in italics; the next single click replaces its document. Editing it, a double click on it or its file, or opening its file from elsewhere keeps it as an ordinary file tab.
- File sidebar: the file browser inside a Files surface. Pinned, it shows beside the document; hidden, the document takes the width. With no document it shows alone.
- Search mode: the File sidebar's filter box with a query starting with `?`: it searches the files' contents (not their names) and shows the matching lines grouped by file in place of the tree, with match-case, whole-word, and regex toggles in the box. Without `?` the box filters names (or completes a path starting with `/`, `~`, `.`).
- Document header: the details above a Files surface's document (name, kind and size, when modified), with the document's controls and `⋯` at the right.
- View mode: how a document shows its text, stepped by one button in its document header (the icon shows the current mode). Preview: rendered and read-only (Markdown). Integrated: the source with every character still there and editable, formatted (bold, sized headings, code boxes). Source: the plain editable source, colours only. A document offers the modes its language supports; Markdown opens in Integrated.
