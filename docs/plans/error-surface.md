# Plan: user-visible error surface

Branch `feat/error-surface` from `develop` (`70ac650`). Owner's decision:
`decisions-pending.md` section 1, "Your answer". This file is deleted in the
last commit of the build.

## Rules applied

| Where the failure happens | Surface |
| :- | :- |
| A form: the floating picker (Save As, Save Copy As, Open), the palette's argument prompt (Rename, New File, New Folder) | Inline error under the field. Confirm is blocked until the input changes. Cancel always works. |
| Text in the editor (language service diagnostics) | Wavy underline under the range: red for errors, yellow for warnings, blue for info and hints. |
| Anything tied to a pane | Toast across the top of that pane, below its tab strip. |
| Anything not tied to a pane | Toast across the top of the workspace area: full window width minus the sidebar. |

Toast levels and colours: info (blue), success (green), warning (orange),
error (red). Errors and warnings stay until dismissed (`×`, or one of the
toast's own choices). Info and success hide after 4 s on screen.

## Sources and the surface each gets

| Source (today) | Surface | Scope |
| :- | :- | :- |
| Save As / Save Copy As fails (`after_pick`, `window.rs:1994`) | Inline in the picker; picker stays open | — |
| Open picker: file won't open (`finish_picker` → `open_file`) | Inline in the picker; picker stays open | — |
| Sidebar open refused: not UTF-8, unreadable, too large (`open_requested_file`, `window.rs:1530`) | Error toast | Pane of the requesting surface |
| `ShowInSidebar` / `OpenUrl` fail (`apply_outcome`, `window.rs:1871`, `:1876`) | Error toast | Pane of the requesting surface |
| Open plugin from the "nothing open" screen (button `:1648`, number key `:3732`) | Error toast | That pane |
| Editor Cmd+S write fails (`EditorView::save`: `if let Ok(true)` drops the error, no stderr at all) | Error toast, choice "Save As…" | Pane of the editor |
| Editor autosave fails (idle tick and `View::autosave` on blur: `.ok()` drops it) | Error toast, keyed per path | Pane of the editor (idle); app (blur/quit, no pane known) |
| Command run from the palette, a keybinding, or a menu fails (`run_command` `:3535`; `palette_click` and the keybinding path discard the `Result` with `let _ =`) | Error toast | Pane the menu opened from, else the focused pane, else app |
| Command run from the argument prompt fails (Rename / New File / New Folder: "name taken", invalid name) | Inline in the prompt | — |
| Argument prompt value doesn't parse (silently kept today) | Inline in the prompt ("expected a number") | — |
| File menu: nothing to paste, Trash refused (via `run_command`, `after_confirm` `:1926`) | Error toast | Menu's pane (paste); app (Trash confirm answered) |
| File-job reports: copy/move failed or left part behind (`print_file_reports` `:1982`) | Error / warning toast | App (the job finished later; its pane may be gone) |
| `after_confirm` / `discard_confirm` errors: Save and Close, Save and Quit, Overwrite (`:1926`, `:1936`) | Error toast | App |
| Config load at startup and on reload (`config.rs:523`, `:534`): unreadable, not valid TOML, dropped or clamped entries | Warning toast (findings only) or error toast (not applied), choice "Open Settings" | App |
| Cmd+, can't open the config (`open_config_editor` `:1373`) | Error toast | App |
| Run Anything can't start (`:3800`) | Error toast | App |
| `[editor] preview_font` not installed (`:5240`) | Warning toast, choice "Open Settings" | App |

Left on stderr, with no window to show them in: window init failure
(`:235`, the event loop exits), render errors (`:533`, per frame), and
file-job reports at exit (`exiting`, `exit_if_quitting`). Agent-run command
failures already go back to the agent as JSON; background file-job reports
from agent-started jobs still toast (they arrive through `file_reports`). Web
host failures already show inline in the web pane; unchanged.

Every toast also prints its message to stderr, as today, so terminal launches
keep their log.

## Design

### Toast model: `crates/app/src/toasts.rs` (new), owned by `AppState`

```rust
pub enum Level { Info, Success, Warning, Error }
pub enum Scope { App, Pane(PaneId) }
pub enum ToastAction { OpenSettings { line: Option<usize> }, ViewButton(SurfaceId, u32) }
pub struct Toast { level, message: String, scope: Scope, key: Option<String>,
                   choices: Vec<(String, ToastAction)>, sticky: bool }
pub struct Toasts { queue: VecDeque<Entry>, next_id: u64 }   // Entry: id + Toast + shown_for: Duration
```

- Lives in `AppState` (`AppState::notify(toast)`, `toasts()`, `toasts_mut()`),
  so `AppState` code (file-job reports, picker, commands) and the window both
  push, and headless tests read the queue. The window only draws and
  hit-tests.
- One FIFO queue for the whole window; only the head shows. Its scope says
  where it draws. When more are queued, the head shows a dim "+N" before `×`.
- `sticky` defaults from level: `Warning`/`Error` true, `Info`/`Success`
  false. A source may override.
- `key` de-duplicates: pushing a toast whose key matches a queued one
  replaces that entry in place (keeps its position; resets `shown_for`).
  Keys: `config`, `preview_font`, `autosave:<path>`. A clean config reload
  replaces a queued `config` toast with a success toast "Settings applied"
  (auto-hides); with none queued, a clean reload shows nothing.
- `tick(now, visible)` adds elapsed time to the head's `shown_for` only while
  `visible`: drawn (no modal or menu open), the window focused, and the
  pointer not over it. A non-sticky head is dropped at `AUTO_HIDE` (4 s).
  `next_deadline()` feeds `about_to_wait`'s wake, as `tooltip_due` does.
- A `Pane` scope whose pane isn't on screen (closed, or in another workspace)
  draws as `App` until it is.
- Cap of 32 queued: past it the oldest non-sticky entry goes, else the
  oldest. The agent channel and headless runs have no window to dismiss
  them.
- `dismiss(id)` and `choose(id, index)`; choosing runs the action, then
  dismisses. Command `toast.dismiss` (palette, bindable, no default chord)
  dismisses the head. Escape is not taken: it belongs to the focused view.

### Rendering and the overlay layers

`Frame` draws overlay text above every overlay quad, so modals can't stack.
Toasts don't need that fixed:

- The toast paints into a `Frame` of its own (as `build_tooltip` does), with
  a `PaintCx` on `Layer::Overlay`, built after the panes and before the
  tooltip (so the toast's `×` tooltip goes through the normal path).
- It draws in a new pass: after the overlay text, before the menu/tooltip
  quads. A fourth `TextRenderer` (`toast_renderer`) holds its text; a
  renderer replays only what it last prepared, so render it only on frames
  that prepared it, as the menu renderer does.
- Pass order becomes: bg quads → base images → base text → top quads →
  overlay images → overlay text → **toast quads → toast text** → menu /
  tooltip quads → menu / tooltip text.
- Views' popups (`PaintCx::above`: hover docs, the TOC panel) sit under the
  toast. The tooltip sits over it.
- While the palette, picker, confirmation, or a menu is open, no toast is
  drawn and its clock stops. Those modals own the pointer and draw in the
  top/overlay layers, so a toast drawn above them couldn't be clicked, and
  one drawn under them would need the per-modal layers. The queued toast
  shows once the modal closes. Per-modal layers stay a TODO entry.

Layout (`toasts::paint(cx, toast, rect, more) -> ToastLayout`, a free function
so tests paint it with a bare `Frame`):

- Pane scope: x = pane content left + margin, width = pane content width −
  2 × margin, top = content top (below the tab strip) + margin. App scope:
  the same against `workspace_rect()` (excludes the sidebar and title bar).
  Margin 6 px × scale. A pane narrower than 24 cells shows its toast app-wide.
- One row per wrapped line (`wrap_words`, as the confirmation does), at most
  3; the last ends in an ellipsis when cut, and the full message is the
  toast's tooltip. Then right-aligned choices, the "+N" count, and `×`
  (`Icon::Close`).
- Fill: the level's colour (new `theme::TOAST_INFO`, `TOAST_SUCCESS`,
  `TOAST_WARNING`, `TOAST_ERROR`), opaque, `pill_radius` corners, a hairline
  border a shade lighter; text `theme::TEXT`. Values tuned at the desktop
  check.

### Hit-testing (UI button convention)

- `×` and each choice: `cx.hovered` → `cx.highlight` on hover, then
  pushed to `self.buttons` as `ButtonAction::Toast(id, ToastHit::Dismiss |
  ToastHit::Choice(i))`. Press arms, release over the same button fires
  (`arm_button` / `fire_button`, unchanged).
- The window keeps `toast_rect` (last frame's). While the pointer is inside it:
  - `button_at_cursor` considers only `ButtonAction::Toast` buttons (view
    buttons registered earlier under the toast would otherwise win);
  - `pane_under_cursor`, `surface_under_cursor`, and
    `focus_pane_under_cursor` return `None`, as over the sidebar, so
    presses, drags, the wheel, focus-follows-mouse, and hover never reach the
    pane under it (a terminal with mouse reporting included);
  - panes paint with the cursor at `(-1, -1)` so nothing under the toast
    lights up.
- A press on the toast body outside its buttons does nothing.

### Panes and views: `Request::Notice`

- New `Request::Notice { level, message, key, choices: Vec<(String, u32)> }`.
  The app scopes it to the requesting surface's pane; a choice calls
  `View::button(id)` on that surface (`ToastAction::ViewButton`).
- `EditorView::save`: on `Err` request an error notice "Couldn't save
  <name>: <error>" with choice "Save As…" (a new editor button id whose `button` handler requests `Request::SaveAs`).
  Idle autosave: the same, keyed `autosave:<path>`, no choice.
- `View::autosave` returns `Result<Option<PathBuf>>`; `autosave_all` toasts
  an `Err` at app scope, keyed per path.

### Config findings

- `core::config::load_or_create` returns `Loaded { config: Option<Config>,
  report: Option<Report> }`. `Report { path, read_error: Option<String>,
  findings: Vec<(line, col, Severity, message)> }`. It prints nothing.
- The app prints the same stderr lines as today and pushes one toast, key
  `config`:
  - not applied (unreadable / not valid TOML): error, "Settings not applied:
    config.toml line L: <message>";
  - findings only: warning, "config.toml: N problems; line L: <first
    message>" (N > 1) or the single finding;
  - choice "Open Settings": `open_settings`, then `EditorView::reveal` at the
    first finding's line.
- Startup findings are pushed once `AppState` exists; the first frame shows
  them.

### Inline errors in the picker

- `FileBrowserView` gains `error: Option<String>` with `set_error` /
  `error()`. In `Mode::Save` it draws one row above the name row in
  `TOAST_ERROR`'s text colour, cut to fit (full text as tooltip), and the
  name field's border turns red. `Mode::PickFile` draws the same row at the
  bottom of the listing.
- Editing the name, moving the selection, or changing directory clears it.
  While it shows, Enter and the picker's confirm button do nothing (the
  button draws dim and doesn't arm). Escape and Cancel close the picker.
- `AppState::finish_picker` acts with the picker still in place and takes
  it only on success. On `Err` it sets the browser's error and returns
  `Ok(None)`, so the picker stays open. The Replace confirmation goes through
  the same path: a failed replace closes the question and shows the error in
  the picker.

### Inline errors in the palette's argument prompt

- `PendingArgs` gains `error: Option<String>`. On the last argument's Enter
  the window runs the command before closing the palette:
  - `Ok`: close.
  - `Err`: keep the palette, pop the last value (the same argument is asked
    again, text kept), set `error`. The prompt draws it in red under the
    input.
- An unparseable value sets `error` ("expected a number", from the arg
  kind) instead of silently staying.
- Editing the input clears `error`. Enter with an error showing and the input
  unchanged does nothing. Escape closes.

### Squiggles

- New `PaintCx::squiggle(x0, x1, bottom, colour)`: a zigzag of `line`
  segments, period 4 px × scale, amplitude 1.5 px × scale, stroke 1 px ×
  scale, opaque (overlapping translucent segments darken at the joints),
  drawn over text (`over_text`), honouring the clip.
- The editor's diagnostic underline (`editor/mod.rs:1579`) calls it in place
  of the flat `fill_top` bar, per visual row as now.
- Severity colours move from `editor/lang.rs` to `plugin::theme`:
  `DIAG_ERROR` red (today's `[240, 100, 100]`), `DIAG_WARNING` yellow (from
  today's amber `[230, 180, 70]` to about `[230, 205, 70]`), `DIAG_INFO` blue
  (unchanged). The hover popup's paragraph colours follow.
- The config file's findings already reach the editor as diagnostics, so an
  open `config.toml` shows them as squiggles.

## Commits

1. `docs(plan): plan the user-visible error surface` (this file).
2. `refactor(core): return config load findings instead of printing them`.
   The app prints the same lines; no behaviour change.
3. `feat(editor): draw diagnostics as wavy underlines`. `PaintCx::squiggle`,
   theme colours, yellow warnings.
4. `feat(app): show failures as toasts`. `toasts.rs`, the toast pass and
   renderer, hit-testing, timers, `toast.dismiss`, and the app-scope sources:
   config load/reload, Cmd+,, Run Anything, preview font, file-job reports,
   confirm results, command failures (focused pane / app).
5. `feat(app): scope toasts to panes and let views raise them`.
   `Request::Notice`, pane placement and fallback, refused opens,
   `ShowInSidebar` / `OpenUrl`, open plugin, file menu pane scope, editor save
   and autosave failures, the `View::autosave` signature.
6. `feat(files): keep the picker open with its error inline`.
7. `feat(palette): show a failed command's error in the argument prompt`.
8. `docs: record the error surface`. `DECISION.md` entry (verbatim owner
   answer as the reason); PLUGINS.md (`Request::Notice`, `PaintCx::squiggle`,
   toasts in "Painting" and "Input", the picker error), SPEC.md rendering model
   (toast pass); delete the TODO entry "PRIORITY: User-visible error surface",
   the "Failures … reach stderr only" bullet under "File and folder context
   menu — remaining", findings item 5 (and its empty "Medium" heading) and
   item 15; add a TODO entry for per-modal overlay layers; delete this plan.

Each commit builds and passes `cargo test --workspace` (per the `build`
skill).

## Tests

- `toasts.rs` units: FIFO order and one head; dismiss advances; a key
  replaces in place; info/success hide after 4 s of visible time; time doesn't
  accrue while hidden, unfocused, or hovered; sticky never hides; a missing
  pane falls back to app; cap drops the oldest non-sticky; `next_deadline`.
- `toasts::paint` on a bare `Frame`: wraps to 3 lines with an ellipsis;
  `×` and choice rects inside the toast; hover highlight only on the hovered
  button; "+N" when more are queued.
- `AppState`: a Save As to an unwritable folder keeps the picker open with
  the error, and a changed name then saves; Cancel closes it; Open of a
  non-UTF-8 file keeps the picker with the error; a failed Replace shows the
  error in the picker; `file.paste` with nothing cut queues an error toast
  scoped to the given pane; a failed copy job's report becomes an app error
  toast; a sidebar open of a non-UTF-8 file queues a toast on that pane.
- `FileBrowserView` (Harness): the error row paints in Save mode; typing
  clears it; Enter with the error showing and the name unchanged requests no
  `Pick`.
- Editor (Harness): extend `diagnostics_are_underlined_and_follow_edits`:
  zigzag segments under the span, red for an error, yellow for a warning, none
  past the span; a wrapped span squiggles on each visual row.
- Editor: Cmd+S on a read-only file requests `Request::Notice` (error) and
  the buffer stays modified.
- `core::config`: `load_or_create`'s report carries line/col/severity for a
  clamp, and `read_error` for an unreadable file.
- Window: `PendingArgs` failure keeps the prompt on the same argument with
  the text and error; editing clears it. The offscreen render test draws a
  toast quad after an overlay quad and reads the toast colour back where they
  overlap.
- Desktop check (owner, `flag-for-owner`): colours, placement under a tab
  strip, toast vs hover popup, picker error row.

## Risks

- Pointer occlusion: a path that bypasses `pane_under_cursor` /
  `surface_under_cursor` would let a click through. Audit every call site;
  the AppState-level tests can't see this, so the desktop check covers it.
- A sticky toast covers the top of its pane (file tabs, document header)
  until dismissed. Intended by the decision; margin keeps the tab strip clear.
- Repeated failures (autosave every idle tick, the 2 s config poll) would
  flood the queue without keys. Every repeating source gets a key.
- `toast_rect` is last frame's, so the first frame of a new toast still
  routes the pointer to the pane under it.
- `window.rs` is 6,549 lines; the model and painting go in `toasts.rs`, and
  the window adds only glue.

## Open questions

1. One queue for the whole window (literal reading of "showing only 1 issue
   at a time"), or one per pane plus one app-wide? Planned: one.
2. Warnings sticky like errors? Planned: yes (a clamp changes behaviour).
3. Toasts hidden under modals with their clock stopped, rather than drawn
   above the dim. Planned: hidden.
4. The palette's argument prompt counts as a form (inline errors). Planned:
   yes.
5. Success toasts: only "Settings applied" after a fix replaces a queued
   config toast. Background copy/move successes stay silent. More wanted?
6. `toast.dismiss` has no default chord. Add one to the new-config bindings?
7. Add "Toast" to GLOSSARY.md? Its rules want the owner to agree new terms.
