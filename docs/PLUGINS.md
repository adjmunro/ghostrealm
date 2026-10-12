# Plugins.md

How content kinds plug into the app. The terminal, editor, and file browser are
built on exactly this API (`crates/app/src/plugins/`); a new kind (Git GUI, web
browser, image viewer, ...) should be too. The API lives in
`crates/app/src/plugin/`.

## Shape

- **`Plugin`**: a content kind, registered once (`plugins::builtin()`). It has an
  id (`terminal`, `editor`, `file_browser`), a title, and opens **`View`**s.
  - `open(&OpenCx)`: a fresh view (new shell, empty buffer, browser at the cwd).
  - `opens_path` / `open_path`: claim a file. Opening a file asks plugins in
    registration order; the first claim wins, so catch-alls (the editor) go last.
  - `pickable`: listed in the "nothing open" picker (number keys follow order).
  - `register_commands`: add palette/keybinding/agent commands (`<id>.new` by
    convention).
  - `config_sections`: the config sections the plugin reads, as catalogue
    entries (type, range, default, description). The config file is checked
    against them and shows them on hover; see "Config" below.
- **`View`**: one surface's content (one tab in a pane). Only `paint` is required;
  every other hook defaults to "not interested". Views live in
  `AppState::views` (headless-safe: the agent channel and tests drive them with no
  window).

The app owns the window, GPU, fonts, layout, focus, tab strips, sidebar, and
overlays. A view owns everything inside its content rect.

## Painting (`PaintCx`)

Immediate mode: each redraw, the active view of each pane gets `paint(cx, rect)`.
A change of `rect` is the resize signal (the terminal resizes its PTY there).

- Quads: `fill`, `fill_alpha` (under text), `fill_top` (over text: cursors,
  carets), `border`, `highlight` (the standard hover/selection look).
- One-off text: `shape` / `shape_spans` → `place`, or `label`. Shaped into a
  per-frame scratch pool; `Font` picks family + italic/bold.
- Rows: `row` (cached by content, always shaped) and `budgeted_row` (cached by
  content, shaped only within the frame's time budget; past it the position redraws
  last frame's row and the frame is re-requested). Use `budgeted_row` for bulk
  content (terminal grid, editor text) and build the key with `row_key` from your
  cells so a cache hit costs no allocation.
- Buttons: `button(rect, id)` registers a hit rect and returns hover; the app
  fires `View::button(id)` on mouse-up over the same button. An icon-only
  button names itself with `tooltip(rect, text)` (shown after a short hover).
- Images: `image(id, version, source, rect, clip)` draws BGRA pixels from an
  `ImageSource` stretched over `rect`, over the layer's fills and under its
  text. The app keeps a texture per `(view, id)` and re-uploads only when
  `version` changes (the web view's page frames). A source may supply mips (so
  it shrinks smoothly) and opt out of blending (`opaque`). `Image` is a decoded
  one: `Image::from_rgba` builds its mips, so call it on a worker; pass its
  `id()` as the version, and `fit` it to keep its aspect ratio.
- Cursor: `cursor(rect, icon)` shows a mouse cursor while `rect` is hovered (a
  link's pointer, a text field's I-beam).
- `request_frame` asks for another paced frame (e.g. eased scrolling).
- `above(|cx| ...)` paints into the overlay pass, over every pane (a view's
  popups, such as the editor's hover docs). While `cx.modal` (a palette, menu,
  picker, or confirmation covers the panes) it paints nothing; close popups
  then, so they don't reappear when the modal goes.
- `tab_strip` says whether the pane shows a strip; a single-tab view may draw its
  own header (the editor's filename ribbon).
- Colours: `cx.chrome` (configurable background/sidebar/accent) and
  `plugin::theme` (fixed UI tokens). Widgets: `plugin::widgets::text_field`.
- Text input: `plugin::text_edit` maps keys to edits through the platform's
  `Keymap` (`edit_for`) and applies them to a `LineEdit` (one line, cursor,
  selection); draw it with `text_field`. Every text input uses it, so they all
  edit alike.

A view hosted in a modal (the floating picker's file browser) paints the same way
into the overlay layer; it doesn't need to know.

## Input (`EventCx`)

- `key(&KeyPress) -> bool`: keys reach the focused view after the app's own chords
  (Cmd+, and `[keybindings]`) and any open overlay. Unbound Cmd chords arrive with
  `mods.super_` set; return `false` to ignore. Nothing falls through to another
  view.
- `mouse(&MouseEvent)`: `Down { clicks }` (the app focuses the pane first and
  counts double-clicks), then `Drag`/`Up { dragged }` for that press wherever the
  cursor goes; `Move` is hover.
- `scroll(pos, dx, dy)`: physical pixels; positive `dy` reveals content above.
- `raw_key(&RawKey) -> bool`: every press *and release* with the platform event
  (physical key, unmodified character), before `key`; return true to take it.
  For views embedding an engine with its own key handling (the web view).
- `focus_changed`, `shown` (the view came on or went off screen: a hidden view
  can pause rendering), `deadline`/`tick` (timers), `pump`/`is_busy` (background work
  and the inbox), `refresh` (background results that aren't output, e.g. a
  language service's diagnostics: repaints without marking the workspace
  unread), `autosave(cfg)` (app blur/quit; the view decides whether it
  autosaves), `path`/`save_as` (Save As), `write_input`/`text`
  (agent channel).
- Effects outside the view go through `cx.request(Request::…)`, applied after
  the call returns: `OpenFile` (tab or split per `[file_browser] open_in`),
  `Saved` (hot-reloads the config), `Busy` (optimistic busy status), `Pick`
  (a view in the floating picker chose a path; the picker confirms with it),
  `SaveAs` (the picker asks where, then calls `save_as`).
- `cx.redraw()` repaints; `cx.request_frame()` repaints on the paced clock.

## Rules

1. Never own a `FontSystem`; the glyph atlas is the render thread's.
2. Keep heavy work (parsing, decoding, walks) off the UI thread: run a worker,
   publish snapshots, wake the UI with `OpenCx::waker` (see `ThreadedTerminal`).
   Anything slow in `paint`, `pump`, or a handler stalls every pane.
3. Keep `paint` cheap when nothing changed: reuse snapshots, key rows by content.
4. Follow the button convention (hover highlight, fire on mouse-up) via
   `PaintCx::button`.

## Config

A plugin's options live in its own `[section]` of the config file. Describe
each in `config_sections` with a `SectionSpec` of `OptionSpec`s
(`ghostrealm_core::config`): its `Kind` (bool, ranged int/number, enum with a
description per value, path, colour, chord), default, and description. The
loader drops or clamps entries that don't fit, and the config file's language
service flags them and shows the docs on hover. A test checks every typed
config field is catalogued and every documented default is the real one.

## Language services

`OpenCx::languages` starts the language service for a file, if it has one
(`crate::languages`). The view owns the returned `ghostrealm_lsp::Client`:
dropping the view shuts the service down, so it runs only while the file is
open. First-party servers implement `ghostrealm_lsp::LanguageServer` and run
in-process; external ones are spawned processes speaking LSP on stdio.

## Testing

`plugin::testing::Harness` paints a view and delivers input with no window or
GPU (`paint`, `key`, `mouse`, `scroll`, `button`, `focus`, `tick`). Every
first-party plugin has tests on it; new plugins should too.

## Not yet

In-process Rust only, compiled into the app. See TODO.md for out-of-tree plugins,
typed per-plugin config (the catalogue entries are the plugin's; the typed
fields still live on `core::Config`), and moving the app's own chrome onto
`PaintCx`.
