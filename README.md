# ghostrealm

A native terminal multiplexer built on `libghostty-vt`: vertical tab sidebar
(email-style inbox), recursive splits with per-pane horizontal tabs, a command
palette, and an agent control channel. macOS first; Linux is a goal (the stack
is cross-platform). Background and rationale live in [`docs/`](docs) —
[GOAL](docs/GOAL.md), [SPEC](docs/SPEC.md), [DECISION](docs/DECISION.md).

## Build & run

Requires Rust and **Zig 0.16.0**, pinned in `mise.toml` (`mise install`). Zig
builds the bundled `libghostty-vt` from source on first build (~2 min, cached).

```bash
mise exec -- cargo run            # open the window
mise exec -- cargo run -- agent   # JSON agent channel on stdin/stdout
mise exec -- cargo run -- dump 'ls -la /'   # headless: print a grid as text
mise exec -- cargo test           # 44 tests
```

Cargo never removes superseded build artifacts: `mise run clean-stale` frees
them (`-- --dry-run` lists what would go). Dev builds keep line tables only; set
`CARGO_PROFILE_DEV_DEBUG=true` for full debug info.

Run from the repo root so `mise` puts Zig on PATH. `.cargo/config.toml` sets
`LIBGHOSTTY_VT_SYS_CPU=native` for local builds.

## Keys (defaults)

| Chord | Action |
|---|---|
| Shift, Shift (double-tap) | command palette (type to filter, ↑/↓, Enter, Esc) |
| Ctrl, Ctrl (double-tap) | run a command in a new workspace |
| Cmd+N | new workspace (pick its directory first) |
| Cmd+T / Cmd+W | new / close tab in the focused pane (in a Files surface, Cmd+W closes the shown file tab first) |
| Cmd+D / Cmd+Shift+D | split left-right / top-bottom |
| Cmd+] / Cmd+[ | focus next / previous pane |
| Cmd+, | open the config file |
| Cmd+Q | quit |
| Cmd+S / Cmd+Shift+S | save / save as (editor; an unsaved buffer asks where) |
| Cmd+Z / Cmd+Shift+Z | undo / redo (editor) |
| Cmd+Shift+C | copy the focused file's absolute path |

Drag the divider between split panes to resize them (it lights up and the cursor
becomes a resize arrow; a pane keeps at least 8 columns or 4 rows). A drag moves
only the split that owns the divider. Double-click a divider to put its split back
to halves. Sizes last for the session.

Text fields and the editor share macOS editing keys: Option+←/→ by word,
Cmd+←/→ to the line's start/end, Cmd+↑/↓ to the start/end, Shift to select with
any of them; Option/Cmd+Backspace delete a word / to the line's start; Cmd+A/C/X/V.
Linux and Windows use Ctrl for words and the clipboard, and Home/End for lines. In
the terminal, the same word/line motions and deletes send readline keys.

A Files surface shows a tab per open file. A single click in its sidebar opens the
file in the preview tab (name in italics), which the next single click reuses; a
double click, an edit, or opening the file any other way keeps it. A tab with unsaved
changes shows a dot where its × is (pane tabs too), × again under the pointer. Its
×, a middle click, or Cmd+W closes one file tab; right-click it for the file menu.

Quitting, or closing a tab, pane, or workspace, with unsaved editor changes asks
first: Save and Quit / Cancel / Quit Without Saving (Save and Close / Close Without
Saving for closes). Enter saves, Esc cancels. A buffer with no file can't be saved
there, so it offers only Cancel and the discard button.

Every open editor checks its file the same way (modification time, size, and
inode, every 2 s while the window is focused and when it gains focus). When another
program wrote the file, an editor without unsaved edits re-reads it (as one undo
step); one with unsaved edits keeps them and shows its file name in amber (in the tab strip too). Saving a file
that changed on disk since the editor loaded or wrote it (Cmd+S, Save and Quit, Save
and Close) asks first: Overwrite / Reload (discards your edits, loads the disk
version and stops the close or quit; one undo step brings the edits back) / Cancel;
there Enter cancels. Autosave never asks and never
overwrites such a file: it skips it and the buffer stays modified. Undoing back past
a reload, or a file that became unreadable as text, counts the same way. Saving a file that
was deleted on disk recreates it without asking.

Right-click a file or folder in a Files surface's sidebar (or empty space in it) or
a file tab, or click a document's file name in its header (or "File actions…" in
its `⋯` menu), for the file menu: New File / New Folder (on a folder or empty space), Reveal in Finder,
Show in File Browser (document), Open in
Default App, Open in Terminal (folders), Cut / Copy / Paste / Duplicate, Copy Path /
Copy Relative Path (to the workspace root), Add to .gitignore / .git/info/exclude
(in a git repository), Rename, Save Copy As (document), and Delete, last under a rule.
Delete moves to the Trash (macOS only for now) and closes the file's tabs (a Files
surface left with none shows its sidebar); it asks
first unless git tracks the file and it is unmodified (a folder: all of it, and it
isn't a repository itself). Paste and Duplicate run in the background, one at a
time in the order given, so a big folder doesn't freeze the window; Delete's git
check runs beside them, and asks first about anything a copy or move is using.
A question that appears when a background check finishes ignores keys for a
moment, and Enter then cancels it, so an Enter typed elsewhere doesn't delete.
Quitting while a copy or move runs asks (after any unsaved changes): Quit
Anyway, or Cancel to let it finish and quit later; the quit goes ahead by itself
if it finishes while asking. A move across filesystems that leaves part of the
original (a file changed while it was copied) keeps that file's editor on it and
says so on stderr. Names are asked for in the palette's argument prompt.
Each item is also a `file.*` command (palette, agent, key bindings) acting on the
focused file, or on its `path` argument (a relative one is taken from the workspace
folder). Cut and Copy keep the
file inside the app: Paste works within ghostrealm, not with Finder.

Cmd is reserved for app shortcuts; every other key goes to the focused terminal.
Click a sidebar row to switch vtab, a pane to focus it, a tab to switch it.
Everything is also reachable from the palette (and the agent).

## Agent channel

`ghostrealm agent` speaks line-delimited JSON — one request and one response per
line — over the same command registry the palette and keybindings use:

```
{"op":"help"}                              # all commands + arg schemas
{"op":"search","query":"split"}            # fuzzy match -> ids
{"op":"set","id":"split.leftright"}        # run a command (file commands reply when done)
{"op":"get","what":"tree"}                 # text dump of vtabs/panes/surfaces
{"op":"get","what":"surface","id":3}       # a surface's grid as text
{"op":"input","text":"ls\r","id":3}        # type into a surface
```

## Layout

```
crates/app        bin: winit + wgpu scene, sidebar/palette/tabs, agent channel, config wiring
crates/core       registry, tab/split tree, inbox status, fuzzy search, config   (headless, tested)
crates/lsp        language services over LSP: first-party server host + editor client
crates/process    long-lived child processes that inherit only their stdio
crates/terminal   the seam: TerminalBackend trait + Grid/Cell/Cursor/KeyPress
crates/terminal-ghostty   libghostty-vt + PTY impl of the seam
```

The backend yields a cell `Grid`; the app draws it (glyphs via glyphon, cell
backgrounds/cursor/UI via an instanced quad pipeline) in one wgpu scene, so
terminals, chrome, and overlays composite together and stay cross-platform.

## Config

`$XDG_CONFIG_HOME/ghostrealm/config.toml` (`~/.config/ghostrealm/config.toml`).
It lists only what you set; every other option uses its default. First run
writes just the default `[keybindings]`, and only commands listed there are
bound. Cmd+, opens it in the editor, where hovering a section or option shows
its docs and bad entries are flagged. Saving hot-reloads it: a flagged entry is
dropped (its default applies) or clamped into range, and a file that doesn't
parse leaves the running config unchanged. An edit from another editor applies
too: the focused window checks the file's modification time and size every 2 s
and when it gains focus. The running config reloads. `ghostrealm lsp config`
serves the same checks and docs to any LSP-capable editor over stdio.
