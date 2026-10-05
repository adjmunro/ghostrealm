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

Run from the repo root so `mise` puts Zig on PATH. `.cargo/config.toml` sets
`LIBGHOSTTY_VT_SYS_CPU=native` for local builds.

## Keys (defaults)

| Chord | Action |
|---|---|
| Shift, Shift (double-tap) | command palette (type to filter, ↑/↓, Enter, Esc) |
| Ctrl, Ctrl (double-tap) | run a command in a new workspace |
| Cmd+N | new workspace (pick its directory first) |
| Cmd+T / Cmd+W | new / close tab in the focused pane |
| Cmd+D / Cmd+Shift+D | split left-right / top-bottom |
| Cmd+] / Cmd+[ | focus next / previous pane |
| Cmd+, | open the config file |
| Cmd+S / Cmd+Shift+S | save / save as (editor; an unsaved buffer asks where) |
| Cmd+Z / Cmd+Shift+Z | undo / redo (editor) |

Text fields and the editor share macOS editing keys: Option+←/→ by word,
Cmd+←/→ to the line's start/end, Cmd+↑/↓ to the start/end, Shift to select with
any of them; Option/Cmd+Backspace delete a word / to the line's start; Cmd+A/C/X/V.
Linux and Windows use Ctrl for words and the clipboard, and Home/End for lines. In
the terminal, the same word/line motions and deletes send readline keys.

Cmd is reserved for app shortcuts; every other key goes to the focused terminal.
Click a sidebar row to switch vtab, a pane to focus it, a tab to switch it.
Everything is also reachable from the palette (and the agent).

## Agent channel

`ghostrealm agent` speaks line-delimited JSON — one request and one response per
line — over the same command registry the palette and keybindings use:

```
{"op":"help"}                              # all commands + arg schemas
{"op":"search","query":"split"}            # fuzzy match -> ids
{"op":"set","id":"split.leftright"}        # run a command
{"op":"get","what":"tree"}                 # text dump of vtabs/panes/surfaces
{"op":"get","what":"surface","id":3}       # a surface's grid as text
{"op":"input","text":"ls\r","id":3}        # type into a surface
```

## Layout

```
crates/app        bin: winit + wgpu scene, sidebar/palette/tabs, agent channel, config wiring
crates/core       registry, tab/split tree, inbox status, fuzzy search, config   (headless, tested)
crates/lsp        language services over LSP: first-party server host + editor client
crates/terminal   the seam: TerminalBackend trait + Grid/Cell/Cursor/KeyPress
crates/terminal-ghostty   libghostty-vt + portable-pty impl of the seam
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
parse leaves the running config unchanged. `ghostrealm lsp config` serves the
same checks and docs to any LSP-capable editor over stdio.
