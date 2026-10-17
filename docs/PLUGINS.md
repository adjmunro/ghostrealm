# Plugins.md

How content kinds plug into the app. The terminal, the Files surface (with its
editor and media documents and file sidebar), and the web view are built on
exactly this API (`crates/app/src/plugins/`); a new kind (Git GUI, web
browser, image viewer, ...) should be too. The API lives in
`crates/app/src/plugin/`.

## Shape

- **`Plugin`**: a content kind, registered once (`plugins::builtin()`). It has an
  id (`terminal`, `files`, `editor`, `file_browser`), a title, and opens **`View`**s.
  - `open(&OpenCx)`: a fresh view (new shell, empty buffer, Files surface at the cwd).
  - `opens_path` / `open_path`: claim a file. Opening a file asks plugins in
    registration order; the first claim wins, so catch-alls (the editor) go last.
    The view it returns is a document: the app shows it in a tab of a Files
    surface (`FilesView::show_doc`), or shows the tab of a file one has
    already (`show_path`). A Files surface's own sidebar opens in that
    surface (`AppState::open_file_from`); any other open
    (`AppState::open_file`) brings into view a Files surface of the
    workspace with a tab of the file, else uses the focused one, else the
    one focused last, else a new one. A single click in the sidebar opens
    the file in the preview tab (title in italics), replacing the document
    there under a new id; nothing is asked, as an edit keeps a preview tab
    (so does a double click on it or its file, or an open from elsewhere).
    Every other open gets a tab of its own, right of the shown one, so no
    open replaces a document with unsaved changes. A document that loses
    the shown tab loses focus (it autosaves if it autosaves then).
    `files` claims images and audio (`MediaView`).
  - `pickable`: listed in the "nothing open" picker (number keys follow order).
    A plugin whose views are only documents (`editor`) or parts of another
    surface (`file_browser`) isn't.
  - `register_commands`: add palette/keybinding/agent commands (`<id>.new` by
    convention).
  - `config_sections`: the config sections the plugin reads, as catalogue
    entries (type, range, default, description). The config file is checked
    against them and shows them on hover; see "Config" below.
- **`View`**: one surface's content (one tab in a pane). Only `paint` is required;
  every other hook defaults to "not interested". Views live in
  `AppState::views` (headless-safe: the agent channel and tests drive them with no
  window).
- **Documents**: a view may host several files, each a `View` of its own
  (file tabs inside one surface). `docs()` lists their `DocId`s (default
  `[DocId::SELF]`: the view is its one document), `active_doc()` is the one
  shown, `doc(id)` / `doc_mut(id)` return a hosted one (`None`: the view
  itself is that document; implement both or neither), and
  `dyn View::document(id)` resolves either case. An id is never reused
  within a view and is not a position: a replaced document (the preview tab
  showing another file) gets a new id, so a question holding one (Save As, a
  save conflict, Close) can't reach a different file. The app asks
  documents, not the view, about files: `path`, `modified`,
  `disk_conflict`, `save_as`, `save_copy`, `reload`, `changed_on_disk`,
  `autosave`. A `Request` names no document: the app applies it to the
  active document (`SaveAs`, `SaveConflict`), except `Close`, which closes
  the whole surface and asks about every unsaved document in it, as the
  pane tab's close button and closing a pane, workspace, or the app do,
  and `CloseDoc(id)`, which closes that one document as Close (Cmd+W)
  does the active one: asks about its unsaved changes, then calls
  `close_doc(id)`, whose answer says whether the surface closes. The view
  decides (a view that is its own document closes at once). The Files
  surface (`plugins::files::FilesView`) is the host: a tab per document
  (`plugins::files::tabs`), the shown one filled (a name two tabs share
  gets, dim after it, the shortest end of its folder's path that tells
  them apart, cut before the name is; a long name ends in an ellipsis; the
  full path is the tab's tooltip). Switching tabs selects the file in the
  sidebar only where its listing shows it with no filter typed (it never
  clears the filter, expands, re-roots, or scrolls there); an open reveals
  it; a file tab's close slot,
  a middle click on it, and Cmd+W close one tab, and closing the shown
  tab shows its right neighbour, else its left. It keeps its sidebar when
  its last document closes (showing only its sidebar, it has none, and
  Close closes it). `files.next_tab` / `files.prev_tab` step through the
  focused surface's tabs. Everything else (input, paint, pump, timers,
  `files_changed`) reaches the view, which passes it on (paint, keys, and
  focus to the shown document only; pump, timers, and `files_changed` to
  every one). `title`, `path`, `write_input`, and `text` default to the
  active document's; `modified` and `marked_changed_on_disk` (the pane
  tab's dot and amber title) to whether any document is.

The app owns the window, GPU, fonts, layout, focus, tab strips, sidebar, and
overlays. A view owns everything inside its content rect.

## Painting (`PaintCx`)

Immediate mode: each redraw, the active view of each pane gets `paint(cx, rect)`.
A change of `rect` is the resize signal (the terminal resizes its PTY there).

- Quads: `fill`, `fill_alpha` (under text), `fill_top` (over text: cursors,
  carets), `border`, `highlight` (the standard hover/selection look).
  `round_rect(r, radius, fill, alpha, border)` draws an antialiased rounded
  rect with an optional `(width, colour, alpha)` border inside it (pills,
  code boxes, focus rings); `alpha` 0 with a border draws only the outline,
  and a square with `radius` half its side is a circle. Snap pill and row
  rects to whole pixels: shaped quads sharing a fractional edge leave a seam.
  `line(a, b, width, colour, alpha)` (round ends) and `bar` (flat ends, a
  rotated rect) draw antialiased strokes, one quad each, centred on the line
  from `a` to `b`: a 1 px line along a whole-pixel coordinate blurs over two
  pixels, so put it on a pixel centre (`n + 0.5`), and draw straight
  guides as snapped `fill`s. A translucent polyline is more opaque where
  its segments overlap, as are overlapping translucent fills: draw
  translucent guides so they don't overlap. Size pills, boxes, and borders
  with `theme::pill_radius`, `box_radius`, `hairline`, and `focus_ring_w`,
  which scale with `cx.ui`.
- `over_text(|cx| ...)` draws every quad painted inside it over the panes'
  text, as `fill_top` does: a control floating on text (a code block's copy
  button). Only base-layer quads move; in a popup (`above`) quads already sit
  under the popup's text. They still draw under the app's later top items
  (focus border, palette, picker dim, tooltips, menus), and text placed inside
  the closure stays under them, so paint the control last.
- Clipping: `with_clip(rect, |cx| ...)` clips every quad painted inside it
  (fills, borders, rounded rects, strokes, icons) and every `button` hit
  rect, nesting by intersection. Clip a scrolled list's rows to the list so
  they don't overdraw a header or take its clicks; a rounded rect keeps its
  shape inside the clip, so never shrink its rect to clip it. Text and
  `image` keep the `clip` each call takes; popups painted `above` escape it.
- Icons: `plugin::icons`, drawn from quads (no image assets or font glyphs).
  `FileKind::of(path)` names a file's kind from its name alone and never
  returns `Folder`; check `is_dir` first and use `FileKind::Folder { open }`
  for a directory (`.git` itself maps to `Git`). `kind.paint(cx, rect)` draws
  it in its colour; `Icon::paint(cx, rect, colour)` draws a UI icon
  (chevrons, sidebar toggle, `⋯`, view modes, copy, check, search, close). An icon
  fills the largest square centred in `rect`; give it about a row's height.
- One-off text: `shape` / `shape_spans` → `place`, or `label`. Shaped into a
  per-frame scratch pool, laid out from the left (right-to-left text too);
  `Font` picks family + italic/bold. `cut_to_fit` ends text too wide for a
  width in an ellipsis, cut at a cluster boundary.
- Rows: `row` (cached by content, always shaped), `row_font` (the same in
  another `Font`: a sidebar's sans names, keyed by content and family), and
  `budgeted_row` (cached by content, shaped only within the frame's time
  budget; past it the position redraws last frame's row and the frame is
  re-requested). Use `budgeted_row` for bulk
  content (terminal grid, editor text) and build the key with `row_key` from your
  cells so a cache hit costs no allocation.
- Styled rows: `budgeted_row_styled` takes `RowStyles { runs, cell_w, scale }`
  (`RowStyles::new(runs, cell_w)`, `.scaled(s)`), one
  `RunStyle` (`BOLD`, `ITALIC`, `UNDERLINE`, `BOX`, combined with `|`) per span,
  keyed with `row_key_styled`. Bold/italic pick the face at shaping (cosmic-text
  uses the nearest weight, and skews an italic when the family has none);
  underline and `BOX` (a rounded box outlined in `theme::CODE_BOX_*`, one per
  run of adjacent boxed spans, padded a little into the cells either side, and
  past the clip's left edge when the row isn't scrolled sideways) are
  quads drawn under the glyphs, `cell_w` per char. A row with no styled span passes `RowStyles::NONE`
  (`budgeted_row` does): it keys, shapes, and draws as before and carries no
  style data. Build `runs` only for rows that use a style
  (`syntax::styled_row_spans` returns it empty otherwise).
- Scaled rows: `RowStyles::scaled(s)` (also on `NONE`) draws the row `s`
  times its shaped size from its top-left: the frame's `TextItem.scale`
  becomes glyphon's `TextArea.scale`, which rasterises the glyphs at that
  size, so the row is still shaped and cached once at the frame's metrics.
  Its decorations measure `cell_w * s` per char and `line_h * s` tall. The
  caller lays out the row `line_h * s` high and its columns `cell_w * s`
  wide.
- Buttons: `button(rect, id)` registers a hit rect and returns hover; the app
  fires `View::button(id)` on mouse-up over the same button. An icon-only
  button names itself with `tooltip(rect, text)` (shown after a short hover).
  A view hosting other views paints each inside `with_id_base(base, ...)`
  and passes its events through `EventCx::with_id_base(base, ...)`: their
  button, image, and menu item ids are shifted by `base`, so the host tells
  them apart in `button(id)` by range.
- Images: `image(id, version, source, rect, clip)` draws BGRA pixels
  (sRGB-encoded, alpha premultiplied in linear light) from an `ImageSource` stretched over `rect`, over the layer's fills and under its
  text. The app keeps a texture per `(view, id)` and re-uploads only when
  `version` changes (the web view's page frames). A source may supply mips (so
  it shrinks smoothly) and opt out of blending (`opaque`). `Image` is a decoded
  one: `Image::from_rgba` builds its mips, so call it on a worker; pass its
  `id()` as the version, and `fit` it to keep its aspect ratio.
- Decoding and playback are `crate::media`, not `PaintCx`: `media::image::decode`
  (file to upright RGBA, shrunk to a cap; hand the pixels to `Image::from_rgba`)
  and `media::audio::AudioPlayer` (a thread that owns the output device and
  closes it when idle). Both go through traits (`ImageDecoder`, `AudioBackend`),
  and only `media::image::image_crate` and `media::audio::rodio_backend` name
  the `image` and `rodio` crates. A view that shows or plays a file calls these
  and never the crates; tests that need a PNG, GIF or WAV use `media::testing`.
- Cursor: `cursor(rect, icon)` shows a mouse cursor while `rect` is hovered (a
  link's pointer, a text field's I-beam).
- `request_frame` asks for another paced frame (e.g. eased scrolling).
- `above(|cx| ...)` paints into the overlay pass, over every pane (a view's
  popups, such as the editor's hover docs). While `cx.modal` (a palette, menu,
  picker, or confirmation covers the panes) it paints nothing; close popups
  then, so they don't reappear when the modal goes.
- Document header: `plugins::files::header::DocHeader` draws a document's
  details (name, kind and size, modified) in its rect, from what
  `refresh(path)` last read (call it when the document shows, saves,
  reloads, or follows its file; never from `paint`); the document lays
  out its controls right to left in `DocHeader::control_row`, then paints
  the header up to them. Unsaved, a dot follows the name, as in the file
  tab; a name too long for the header ends in an ellipsis before it. An `EditorView` draws one when its host sets
  `set_header(true)` (the Files surface does); `MediaView` always does.
- Colours: `cx.chrome` (configurable background/sidebar/accent) and
  `plugin::theme` (fixed UI tokens). Widgets: `plugin::widgets::text_field`.
- Text input: `plugin::text_edit` maps keys to edits through the platform's
  `Keymap` (`edit_for`) and applies them to a `LineEdit` (one line, cursor,
  selection); draw it with `text_field`. Every text input uses it, so they all
  edit alike (typing `(`, `[`, or `{` over a selection wraps it, unless the
  field opts out with `LineEdit::with_plain_brackets`; the editor also
  auto-closes brackets, see `text_edit`'s docs). `text_field` returns the
  `FieldGeom` it drew; keep it in a `widgets::FieldMouse`, paint it each frame
  (`painted`, which also shows the I-beam cursor over it, or `clear` while
  the field takes no input), and pass mouse events
  to `FieldMouse::event` first: click, drag, Shift+click, double-click (word),
  and triple-click (all) then edit the `LineEdit` like the editor's.

A view hosted in a modal (the floating picker's file browser) paints the same way
into the overlay layer; it doesn't need to know.

## Input (`EventCx`)

- `key(&KeyPress) -> bool`: keys reach the focused view after the app's own chords
  (Cmd+, and `[keybindings]`) and any open overlay. Unbound Cmd chords arrive with
  `mods.super_` set; return `false` to ignore. Nothing falls through to another
  view.
- `mouse(&MouseEvent)`: `Down { clicks, button }` (the app focuses the pane
  first and counts double-clicks per button), then `Drag`/`Up { dragged }` for
  that press wherever the cursor goes; `Move` is hover, and `Leave` follows the
  last `Move` when the pointer moves on to another view or the app's chrome, a
  modal (palette, menu, confirmation, picker) takes the pointer, or the pointer
  leaves the window (on release, if a press was held). Right and middle presses
  arrive too (a right press before `context_menu`), outside the tab strip; a
  view that handles only the left button returns early on
  `event.is_other_button()`. A new press of any button first ends a press
  still held.
- `scroll(pos, dx, dy)`: physical pixels; positive `dy` reveals content above.
  `cx.mods` carries the modifiers held during `mouse` and `scroll` too (the
  terminal forwards the pointer to a program that enabled mouse tracking unless
  Shift is held).
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
- File menu: `context_menu(pos)` is a right-click inside the view; a view with a
  file or folder to act on files `Request::FileMenu { target, at, align }`
  (the file browser on its rows and empty space, a document on its header,
  the Files surface on a file tab;
  its file name button and its `⋯` menu's "File actions…" file the same
  request, the latter right-aligned at `⋯`). `subject` is the file or folder the `file.*`
  commands act on when given no `path` (default: `path`; the browser's is its
  selected entry). `save_copy` backs Save Copy As. `files_changed(moved)` hears
  that the app created, copied, moved, renamed, or deleted files (a copy or move
  once the file worker, `file_jobs`, finishes it, which may be after the
  command returned): a listing re-reads, a view of a moved file follows it
  (`file_ops::follow`).
  `changed_on_disk(path)` is asked, for each open document's `path`, to check
  whether another program wrote the file (the app polls every 2 s while the
  window is focused and on window focus): a view showing it with no unsaved
  changes re-reads it; one with unsaved changes keeps them and marks itself.
  It returns whether what the view shows changed; `marked_changed_on_disk`
  says it marked itself (the tab title shows amber). `disk_conflict` says
  writing the file now would overwrite a version the view hasn't seen (the file
  changed since the view loaded or wrote it, or its text predates a reload; a
  deleted file is not one), and `reload` replaces the content with the disk
  version. The app asks Overwrite / Reload / Cancel before a Save and Close /
  Save and Quit writes such a view and when a view files
  `Request::SaveConflict`; Reload stops the close or quit. Autosave (`autosave`, idle, blur) must
  skip a view with a `disk_conflict` rather than ask.
- Effects outside the view go through `cx.request(Request::…)`, applied after
  the call returns: `OpenFile` (in a Files surface, as for `opens_path`),
  `Saved` (hot-reloads the config), `Busy` (optimistic busy status), `Pick`
  (a view in the floating picker chose a path; the picker confirms with it),
  `SaveAs` (the picker asks where, then calls `save_as`), `SaveConflict` (an
  explicit save found the file changed on disk: the app asks before writing),
  `FileMenu` (open the file menu on a target), `Menu` (open the view's own
  menu), `Close` / `CloseDoc` (see Documents above).
- View menu: `Request::Menu { items, at, align }` opens a menu of the view's
  own with its top edge at `at` (physical px) and its left or right edge
  there per `align` (`MenuAlign::Right` for a button at the view's right end,
  so the menu stays over the view); the app keeps it on screen, draws it
  above every overlay (the picker's view can open one), and runs it like the
  file menu (hover highlight, fires on mouse-up). Choosing an item calls
  `View::button(id)` with the item's `id`, so an item and a header icon that
  do the same thing share an id. `MenuItem::new(id, label)`, then
  `.check(on)` (a toggle: a check mark when on), `.with_hint(text)` (dim,
  right-aligned: a shortcut or value; cut short on a narrow window),
  `.enable(false)` (dim, not choosable); `MenuItem::RULE` separates sections.
  Build the items when the menu opens, so checks show the current state. A
  menu with no enabled item doesn't open; an open menu closes when a
  confirmation appears or the picker opens or closes under it. The editor
  header's `⋯` lists every header option this way, and the file browser's
  `⋯` what its listing shows.
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

## Syntax highlighting

`ghostrealm_core::highlight` is a registry of `Highlighter`s keyed by file
extension (`for_path`, `for_extension`; the list is `highlight::ALL`). A
highlighter turns one line plus the `State` carried from the line before into
contiguous `Span`s of palette-free `Token`s (`Comment`, `Heading`, `Key`,
`Marker`, `Strong`, `Emphasis`, `Code`, `Link`, ...). `syntax::row_spans` maps
them to colours (`syntax::color`) and slices them to a visual row, ready for
`PaintCx::row`/`budgeted_row`; `syntax::styled_row_spans` also maps them to
`RunStyle`s (Strong/Heading bold, Emphasis italic, Link underline, Code boxed).
`Highlighter::block_row` places a row in a block drawn as a unit (a fenced code
block's opening fence, body, or closing fence): the editor draws each block,
fences included, as one rounded, outlined box the text area's width (open at
an edge where the block runs past the viewport). While the pointer is over a
block, a copy button floats over the text at its top right; it copies the body
without the fences, the opening fence's quote markers, or its container's
indent, and shows a check mark for a moment after; Markdown carries a fence's language in its state and highlights the
body with that language's line-local highlighter. A view that edits text keeps a `StateCache` (only
for a highlighter whose `carries_state` is true; report edits with `dirty_from`)
so a scrolled row knows its start state without re-highlighting the file, and a
`SpanCache` so lines still on screen aren't re-highlighted each frame. A view that
reads a file once runs `highlight_lines` on its worker.
A highlighter must bound its work per line: a paint highlights every visible line.
`Highlighter::comment` declares the language's line and block comment tokens
(`CommentSyntax`); `comment_on` gives a line's, which differs inside an embedded
language (a Markdown code block), and `comment_lead` the chars a comment goes
after there (a quoted block's `>`). A language with no highlighter yet declares its
comment tokens in `highlight::LANGUAGES` (found by fence word or extension with
`language`). The editor's `editor.toggle_comment` (bound to `cmd+/` in a new
config file, never implicitly) reads them, taking an embedded language's only
when every selected line is inside one block of it.
To add a language, implement `Highlighter` in a module under `core/src/highlight/`,
list it in `ALL`, and drop its `LANGUAGES` entry.

## Testing

`plugin::testing::Harness` paints a view and delivers input with no window or
GPU (`paint`, `key`, `mouse`, `scroll`, `button`, `focus`, `tick`). Every
first-party plugin has tests on it; new plugins should too.

## Not yet

In-process Rust only, compiled into the app. See TODO.md for out-of-tree plugins,
typed per-plugin config (the catalogue entries are the plugin's; the typed
fields still live on `core::Config`), and moving the app's own chrome onto
`PaintCx`.
