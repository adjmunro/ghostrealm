# Plan: user-visible error surface

Branch `feat/error-surface` from `develop` (`70ac650`). Owner's decision:
`decisions-pending.md` section 1, "Your answer" (read that section, but don't
edit the file). The last commit of the build deletes this plan. Line numbers
are against `70ac650`.

## Rules applied

| Where the failure happens | Surface |
| :- | :- |
| A form: the floating picker (Save As, Save Copy As, Open), the palette's argument prompt (Rename, New File, New Folder) | Inline error under the field. Confirm is blocked until the input changes. Cancel always works. |
| Text in the editor (language service diagnostics) | Wavy underline under the range: red for errors, yellow for warnings, blue for info and hints. |
| Anything tied to a pane | Toast across the top of that pane, below its tab strip. |
| Anything not tied to a pane | Toast across the top of the workspace area (window width minus the sidebar), below the top row of panes' tab strips. |

Toast levels and colours: info (blue), success (green), warning (orange),
error (red). Errors and warnings stay until dismissed (`×`, or one of the
toast's own choices). Info and success hide after 4 s on screen. One
window-wide FIFO queue; only its head shows.

## Sources and the surface each gets

| Source (today) | Surface | Scope |
| :- | :- | :- |
| Save As / Save Copy As fails (`after_pick`, `window.rs:1994`) | Inline in the picker; picker stays open | — |
| Open picker: file won't open (`finish_picker` → `open_file`) | Inline in the picker; picker stays open | — |
| Sidebar open refused: not UTF-8, unreadable, too large (`open_requested_file`, `window.rs:1530`) | Error toast | Pane of the requesting surface |
| `ShowInSidebar` / `OpenUrl` fail (`apply_outcome`, `window.rs:1871`, `:1876`) | Error toast | Pane of the requesting surface |
| Open plugin from the "nothing open" screen (button `:1648`, number key `:3732`) | Error toast | That pane |
| Editor Cmd+S write fails (`EditorView::save`, `editor/mod.rs:820`: `if let Ok(true)` drops the error; nothing reaches stderr) | Error toast, key `save:<path>`, choice "Save As…" | Pane of the editor's surface |
| Editor idle autosave fails (`tick` → `save_if_modified`, `editor/mod.rs:643`: `.ok()` drops it) | Error toast, key `autosave:<path>`, choice "Save As…" | Pane of the editor's surface |
| Editor autosave on losing focus fails (`focus_changed`, `editor/mod.rs:1916-1920` → `save_if_modified`) | Same as idle autosave | Same |
| App-driven autosave fails (blur, quit: `autosave_all`, `app_state.rs:985-995` → `View::autosave`) | Same as idle autosave | Pane of that surface (the loop knows its `SurfaceId`) |
| Command run from the palette, a keybinding, or a menu fails (`run_command`, `:3535`) | Error toast | Pane the menu opened from, else the focused pane, else app |
| Palette click (`:2356`) and bound chord (`:3702`) call `registry.execute` with empty `Args` and drop the `Result` | Routed through `begin_command` (prompt for required args, else run), so failures toast as above | As above |
| Command run from the argument prompt fails (Rename / New File / New Folder: "name taken", invalid name) | Inline in the prompt | — |
| Argument prompt value doesn't parse (silently kept today) | Inline in the prompt ("expected a number") | — |
| Trash refused after its confirmation (`after_confirm`, `:1926`, wrongly labelled `save:`) | Error toast | App |
| File-job reports: copy/move failed or left part behind (`print_file_reports`, `:1982`) | Error / warning toast | App (the job finished later; its pane may be gone) |
| `after_confirm` / `discard_confirm` errors: Save and Close, Save and Quit, Overwrite (`:1926`, `:1936`) | Error toast | App |
| Config load at startup and on reload (`config.rs:523`, `:534`): unreadable, not valid TOML, dropped or clamped entries | Error toast (not applied) or warning toast (findings), key `config`, choice "Open Settings" | App |
| A clean config load after one with problems | Success toast "Settings applied", key `config` | App |
| Cmd+, can't open the config (`open_config_editor`, `:1373`) | Error toast | App |
| Run Anything can't start (`:3800`) | Error toast | App |
| `[editor] preview_font` not installed (`:5240`) | Warning toast, key `preview_font`, choice "Open Settings" | App |

These stay on stderr only:
- Window init failure (`:235`): the event loop exits, so there's no window
  to show it in.
- Render errors (`:533`): they repeat every frame.
- File-job reports printed at exit (`exiting`, `exit_if_quitting`).

Failures of commands the agent runs go back to it as JSON. Web host failures
already show inline in the web pane. Neither changes.

Every toast also prints its message to stderr, as today, so terminal launches
keep their log.

## Design

### Toast model: `crates/app/src/toasts.rs` (new), owned by `AppState`

```rust
pub enum Level { Info, Success, Warning, Error }
pub enum Scope { App, Pane(PaneId) }
pub enum ToastAction {
    /// Open the config file, caret at this line (1-based) when given.
    OpenSettings { line: Option<usize> },
    /// Ask where to save this document (the Save As picker).
    SaveAs(DocRef),
}
pub struct Toast { level: Level, message: String, scope: Scope, key: Option<String>,
                   choices: Vec<(String, ToastAction)>, sticky: bool }
pub struct Toasts { queue: VecDeque<Entry>, next_id: u64, windowed: bool }
// Entry: id: u64, toast: Toast, shown_for: Duration
```

- `AppState` owns it: `notify(toast)`, `toasts()`, `choose_toast(id, i)`,
  `dismiss_toast(id)`. `AppState` code (file-job reports, the picker,
  autosave, view requests) and the window both push. Headless tests read the
  queue. The window only draws, hit-tests, and ticks.
- One FIFO queue for the window. The head shows at its scope's place. When
  more are queued, the head shows a dim "+N" before `×`.
- `sticky` defaults from the level: `Warning`/`Error` true, `Info`/`Success`
  false.
- De-duplication:
  - A toast with a `key` replaces the queued entry with that key, in place:
    same position, `shown_for` reset.
  - A toast without a key replaces a queued entry with the same scope and
    message.
  - Keys in use: `config`, `preview_font`, `save:<path>`, `autosave:<path>`.
  - So Cmd+S five times on a read-only file, the 2 s config poll, and
    autosave on every idle tick each leave one entry.
- Nothing is ever dropped while the app has a window. `AppState::set_windowed(true)`
  (the GUI calls it beside turning on `guard_unsaved`) marks that. Without a
  window (`ghostrealm dump`, tests that don't opt in) nobody can dismiss, so
  the queue keeps the latest 32.
- `tick(now, visible)` adds elapsed time to the head's `shown_for` only
  while `visible`:
  - drawn (no palette, picker, confirmation, or menu open);
  - the window focused;
  - the pointer not over it.

  A non-sticky head is dropped at `AUTO_HIDE` (4 s). `next_deadline(now)`
  feeds `about_to_wait`'s wake, as `tooltip_due` does.
- A `Pane` scope whose pane isn't on screen draws as `App` until it is: the
  pane closed, sits in another workspace, or is narrower than 24 cells.
- `choose_toast(id, i)` runs the action:
  - `SaveAs(doc)` calls `open_save_dialog(doc, Pick::SaveAs(doc))` (made
    `pub(crate)`) when `doc` still exists. Then the toast is dismissed.
  - `OpenSettings { line }` gets `config::ensure_file()`, then
    `open_settings(path)`, then `EditorView::reveal(line - 1, 0..0)` on the
    settings document when `line` is given. On success the toast is
    dismissed. If `ensure_file` returns `None` or `open_settings` fails
    (`app_state.rs:930`: the path won't open as text), the head is replaced
    in place by an error toast "Couldn't open <path>: <error>" with no
    choices. It keeps the original message after a blank line, so the
    finding isn't lost.
- Command `toast.dismiss` dismisses the head. It shows in the palette and can
  be bound, with no default chord. Escape is not taken: it belongs to the
  focused view.

### Views raise toasts: `Request::Toast`

```rust
Request::Toast { level: Level, message: String, key: Option<String>,
                 choices: Vec<ToastChoice>, doc: Option<DocId> }
pub enum ToastChoice { SaveAs }
```

- The view names no button ids, so the app never has to route a toast choice
  back through a host's id ranges. `EventCx::with_id_base` rebases only
  `Request::Menu` ids, and `FilesView::from_button` would misroute a raw id.
- `doc` is filled in by the host. A new `EventCx::with_doc(id, f)` sets
  `doc` on each `Request::Toast` raised inside `f` that has none, the way
  `with_id_base` rewrites menu ids. `FilesView` wraps every call it forwards
  to a hosted document with it, using that document's own `DocId` (`t.id`):
  `tell` (focus), `on_part(Part::Doc)` (keys, mouse, buttons), and `tick`
  (every document, not just the active one).
- The window passes each such request to `AppState::toast_request(sid, req)`,
  which builds the toast:
  - scope: the pane holding `sid`;
  - doc: `(sid, doc.unwrap_or(DocId::SELF))`;
  - choices: `ToastChoice::SaveAs` → `ToastAction::SaveAs(that DocRef)`.

  So the failed document gets the picker, whichever document is shown by
  then.

### Rendering and the overlay layers

`Frame` draws overlay text above every overlay quad, so modals can't stack.
Toasts avoid needing that fixed:

- The toast paints into a `Frame` of its own (as `build_tooltip` does), with
  a `PaintCx` on `Layer::Overlay`. It is built after the panes, sidebar, and
  title bar, and before `build_tooltip`.
- It draws in a new pass after the overlay text, before the menu and tooltip
  quads. A fourth `TextRenderer` (`toast_renderer`) holds its text. A
  renderer replays what it last prepared, so render it only on frames that
  prepared it, as the menu renderer does.
- Pass order becomes: bg quads → base images → base text → top quads →
  overlay images → overlay text → **toast quads → toast text** → menu /
  tooltip quads → menu / tooltip text. The quad buffer is
  `bg ++ top ++ toast ++ menu`. A pure `pass_ranges(n_bg, n_top, n_toast,
  n_menu)` returns the four instance ranges, and `render` draws from them.
- Views' popups (`PaintCx::above`: hover docs, the TOC panel) sit under the
  toast. The tooltip sits over it.
- While the palette, picker, confirmation, or a menu is open, no toast is
  drawn and its clock stops. Those modals own the pointer and draw in the
  top and overlay layers. A toast drawn above them couldn't be clicked, and
  one drawn under them would need the per-modal layers. The queued toast
  shows once the modal closes. Per-modal layers become a TODO entry.

Layout: `toasts::place(scope_rect, ..)` and `toasts::paint(cx, toast, rect,
more) -> ToastLayout { rect, buttons: Vec<(Rect, ToastHit)>, tooltip }` are free
functions, so tests paint them with a bare `Frame`.

- Pane scope: the pane's content rect (below its tab strip). Margin
  6 px × scale; width = content width − 2 × margin.
- App scope: `workspace_rect()`, with its top moved down past the tallest tab
  strip of the panes whose top edge is the workspace's top. Otherwise the
  toast would cover the top-row panes' strips (render `:4244-4257`). When
  there is no macOS title strip, the sidebar toggle floats at the workspace's
  top corner (`build_sidebar_edge_toggle`, `:2684`). Where that toggle's rect
  overlaps, inset the toast's left or right edge past it. The same margin
  applies.
- Content:
  - one row per wrapped line (`wrap_words`, as the confirmation does), at
    most 3;
  - the last line ends in an ellipsis when cut, and the full message becomes
    the toast's tooltip;
  - then, right-aligned: the choices, "+N", and `×` (`Icon::Close`).
- Fill: the level's colour, opaque, `pill_radius` corners, with a hairline
  border a shade lighter. New tokens `theme::TOAST_INFO`, `TOAST_SUCCESS`,
  `TOAST_WARNING`, `TOAST_ERROR`. Text is `theme::TEXT`. Values get tuned at
  the desktop check.

### Pointer handling (UI button convention)

- `×` and each choice light up with `cx.highlight` while hovered. They go
  into `self.buttons` as `ButtonAction::Toast(id, ToastHit::Dismiss |
  ToastHit::Choice(i))`. A press arms, and a release over the same button
  fires (`arm_button` / `fire_button`, unchanged).
- The window keeps `toast_rect: Option<Rect>` from the last frame. Every
  pointer decision goes through pure functions, in a new module
  `crates/app/src/pointer.rs`, that take a `Gate { modal: bool, picker:
  bool, confirm: bool, toast: Option<Rect> }`:
  - `button_at(gate, &buttons, pos)` replaces the body of
    `button_at_cursor`. Inside the toast it considers only `Toast` buttons,
    because view buttons registered earlier under the toast would otherwise
    win.
  - `hover_index(gate, &buttons, pos)` replaces the `position` in
    `update_button_hover` (`:1666`), checking toast buttons first. Under the
    toast nothing else lights up, so no highlight goes stale.
  - `covered(gate, pos)` is true inside the toast. These return `None` when
    it is:
    - `divider_under_cursor` (`:1754`), so `press_divider`, which runs
      before `on_click`, can't start a drag under the toast;
    - `pane_under_cursor`, `surface_under_cursor`, `htab_under_cursor`, and
      `focus_pane_under_cursor`.

    So presses, drags, hover, focus-follows-mouse, and terminal mouse
    reporting never reach the pane.
  - `wheel_target(gate, pos, under, focused)`: no target inside the toast.
    Today the wheel falls back to the focused surface when nothing is under
    the pointer (`:2613`).
- Panes paint with the cursor at `(-1, -1)` while it is inside
  `toast_rect`, so nothing under the toast lights up.
- Tooltips: `build_tooltip` reads `frame.tooltip` (`:3049-3052`). While the
  pointer is inside the toast, the frame's tooltip is replaced by the toast
  frame's own (the `×` tooltip, or the full message over a cut one), or by
  none. Chrome and view tooltips under the toast body (the tab strip's,
  `:4376`) don't show.
- A press on the toast body outside its buttons does nothing.
- The first frame a new toast shows, `toast_rect` is still the previous
  frame's. The toast draws the same frame its event arrived in, and the next
  pointer event sees the new rect.

### Config findings

- `core::config::load_or_create` returns `Loaded { config: Option<Config>,
  report: Report }`, where `Report { path, read_error: Option<String>,
  findings: Vec<(usize, usize, Severity, String)> }` (line, col). It prints
  nothing.
- The window prints the same stderr lines as today. When the report has
  problems, it pushes one toast with key `config`:
  - not applied (unreadable, or not valid TOML: `config` is `None`): error,
    "Settings not applied: config.toml line L: <message>";
  - findings only: warning, "config.toml: N problems. Line L: <first
    message>" when N > 1, else the one finding;
  - choice "Open Settings" with the first finding's line.
- The window keeps `config_problems: bool`, true when the last load had
  problems. A clean load while it is true pushes a success toast "Settings
  applied" with key `config`: it replaces a queued `config` toast, or
  queues fresh when the user already dismissed or chose "Open Settings" on
  the first. A clean load while it is false shows nothing.
- Startup findings are pushed once `AppState` exists, so the first frame
  shows them.

### Inline errors in the picker

- `FileBrowserView` gains `error: Option<String>`, with `set_error` and
  `error()`.
  - `Mode::Save` draws the error as one row above the name row, in
    `TOAST_ERROR`, cut to fit with the full text as its tooltip. The name
    field's border turns red.
  - `Mode::PickFile` draws the same row at the bottom of the listing.
- Editing the name, moving the selection, or changing directory clears it.
  While it shows, Enter and the picker's confirm button do nothing: the
  button draws dim and doesn't arm. Escape and Cancel close the picker.
- `AppState::finish_picker` acts with the picker still in place and takes it
  only on success. On `Err` it sets the browser's error and returns
  `Ok(None)`, so the picker stays open. The Replace confirmation goes through
  the same path: a failed replace closes the question and shows the error in
  the picker.

### Inline errors in the palette's argument prompt

- Extract the prompt's Enter step from `palette_arg_key` (`:3842`) into
  `PendingArgs::enter(&mut self, input: &str) -> ArgStep`, with
  `ArgStep::{Next, Run(String, Args), Retry}`. Add
  `PendingArgs::fail(&mut self, msg)`, which pops the last value so the same
  argument is asked again, and sets `error`.
- `PendingArgs` gains `error: Option<String>` and `failed_input:
  Option<String>`.
  - An unparseable value sets `error` (for example "expected a number", from
    the arg kind).
  - `Run`: the window runs the command before closing the palette. On `Ok`
    it closes. On `Err` it keeps the palette, calls `fail`, and keeps the
    input text.
  - The prompt draws `error` in red under the input.
  - While `error` is set and the input equals `failed_input`, `enter`
    returns `Retry` and nothing runs. Editing clears `error`. Escape closes.
- `begin_command_with` splits into the pure `command_entry(meta, &preset) ->
  Entry::{Run, Prompt(specs)}` and the window glue. `palette_click`
  (`:2356`) and the bound-chord path (`:3702`) call `begin_command`, so a
  command with required args opens the prompt instead of failing with a
  missing argument.

### Autosave and save results

- `EditorView::save_if_modified` returns `Result<Option<PathBuf>>`.
- `View::autosave` returns `Result<Option<PathBuf>>`. `autosave_all`
  iterates `views.iter_mut()`, so it knows each `SurfaceId`, and toasts an
  `Err` with key `autosave:<path>` and choice "Save As…" (`SaveAs((sid,
  doc))`) at that surface's pane.
- `EditorView::save` (Cmd+S), `tick`'s idle autosave, and `focus_changed`
  raise `Request::Toast` on `Err`: error, "Couldn't save <name>: <error>",
  key `save:<path>` or `autosave:<path>`, choice `ToastChoice::SaveAs`. The
  buffer stays modified.

### Squiggles

- New `PaintCx::squiggle(x0, x1, bottom, colour)` draws a zigzag of `line`
  segments over text (`over_text`), honouring the clip:
  - period 4 px × scale, amplitude 1.5 px × scale, stroke 1 px × scale;
  - opaque, because overlapping translucent segments darken at the joints.
- The editor's diagnostic underline (`editor/mod.rs:1579`) calls it instead
  of the flat `fill_top` bar, per visual row as now.
- Severity colours move from `editor/lang.rs` to `plugin::theme` as
  `DIAG_ERROR`, `DIAG_WARNING`, and `DIAG_INFO`. The hover popup's paragraph
  colours follow.
- `DIAG_WARNING` changes from amber `[230, 180, 70]` to yellow, about
  `[230, 205, 70]`. Error red and info blue stay as they are.
- `config.toml`'s findings already reach the editor as diagnostics, so the
  open file shows them as squiggles.

### Agent and file-job reports

- `agent.rs:93-97` waits for every file job and drains every report into the
  agent's reply. A job the user started that finishes during an agent call
  would be lost.
- Fix: `FileJobs` numbers jobs in submission order. `FileReport` carries its
  `job` number. The agent takes `state.file_job_mark()` before `execute` and,
  after `wait_file_jobs`, calls `take_file_reports_since(mark)`, which
  returns only its own jobs' reports. Earlier jobs' reports stay queued, and
  the window toasts them.

## Commits

Topological changes first, each behaviour in its own commit. Each one builds
and passes `cargo test --workspace` (see the `build` skill).

1. `docs(plan): plan the user-visible error surface` (this file).
2. `refactor(editor): move diagnostic colours to the theme`. Same values.
3. `refactor(core): return config load findings instead of printing them`.
   The window prints the same lines.
4. `refactor(app): return autosave results`. Covers `save_if_modified` and
   `View::autosave` → `Result`. Callers print an `Err` to stderr; no other
   change.
5. `refactor(app): return the missing preview font instead of printing it`.
   `pin_preview_serif` (`:5221`) returns the missing name, and the caller
   prints it.
6. `refactor(app): pull pointer decisions and the argument prompt step into
   pure functions`. Adds `pointer.rs` (with no toast yet: `Gate.toast` is
   always `None`), `PendingArgs::enter`, and `command_entry`. No behaviour
   change.
7. `fix(app): name the failed action in confirmation errors`. In
   `after_confirm`, a Trash failure is no longer labelled `save:`.
8. `fix(app): run palette clicks and bound chords through the argument
   prompt`.
9. `fix(agent): reply with only the agent's own file-job reports`.
10. `feat(editor): draw diagnostics as wavy underlines`. `PaintCx::squiggle`
    and yellow warnings.
11. `feat(app): add the toast queue, pass, and pointer handling`. Adds
    `toasts.rs`, `AppState::notify` and friends, the toast pass and
    renderer, `pass_ranges`, both placements, the pointer gate, the tooltip
    swap, the clock and wake, `toast.dismiss`, and `ButtonAction::Toast`.
    Nothing calls `notify` yet outside tests.
12. `feat(app): toast config problems`. Config load and reload, "Open
    Settings", "Settings applied", `preview_font`, Cmd+,.
13. `feat(app): toast failed commands and background file jobs`.
    `run_command` (menu pane, else the focused pane, else app), Run Anything,
    file-job reports, `after_confirm` / `discard_confirm`.
14. `feat(app): toast refused opens on their pane`. Covers
    `open_requested_file`, `ShowInSidebar`, `OpenUrl`, and open plugin.
15. `feat(editor): toast save and autosave failures`. Adds `Request::Toast`,
    `ToastChoice`, `EventCx::with_doc`, the `FilesView` wrapping, and
    `AppState::toast_request`, and wires the editor and `autosave_all`.
16. `feat(files): keep the picker open with its error inline`.
17. `feat(palette): show a failed command's error in the argument prompt`.
18. `docs: record the error surface`:
    - `DECISION.md`: an entry quoting the owner's answer verbatim as the
      reason.
    - `GLOSSARY.md`: add "Toast: a coloured notice across the top of the pane
      it concerns, or of the workspace area for the whole app. Errors and
      warnings stay until dismissed; info and success hide on their own. One
      shows at a time, the rest wait in order."
    - `PLUGINS.md`: `Request::Toast` and `EventCx::with_doc` (under Input),
      `PaintCx::squiggle` (under Painting), the picker error, and toasts
      drawing over `above` popups.
    - `SPEC.md`: the toast pass in the rendering model.
    - `TODO.md`:
      - delete "PRIORITY: User-visible error surface", and the "Failures
        (name taken, nothing to paste, Trash refused) reach stderr only"
        bullet under "File and folder context menu — remaining";
      - add per-modal overlay layers;
      - add, out of scope here: `terminal-ghostty/src/lib.rs:377`'s
        per-frame snapshot message stays on stderr;
        `plugins/web/mod.rs:1032` and `:1605` drop the `Result`s of
        navigate, back, forward, and reload; `save_views`
        (`app_state.rs:735-750`) loses `written` when a later save fails.
    - Delete findings item 5 (and the "Medium" heading it leaves empty) from
      `stack-review-findings.md`, and item 15 from
      `stack-review-findings-2.md`.
    - Delete this plan.

## Tests

`WindowState` needs a GPU, so window logic is tested through the pure
functions above.

- `toasts.rs`:
  - FIFO order with one head; dismiss advances;
  - a key replaces in place, and so do the same scope and message without a
    key;
  - info and success hide after 4 s of visible time; time doesn't accrue
    while hidden, unfocused, or hovered; sticky toasts never hide;
  - windowed: 40 sticky errors all stay; headless keeps the latest 32;
  - a missing pane falls back to app;
  - `next_deadline`.
- `toasts::place`: an app toast sits below the top row's tallest strip and
  clears the floating sidebar toggle; a pane toast sits below its pane's
  strip; a narrow pane falls back to app.
- `toasts::paint` on a bare `Frame`: wraps to 3 lines with an ellipsis and
  the full text as tooltip; the `×` and choice rects are inside the toast;
  only the hovered button lights up; "+N" shows when more are queued.
- `pointer.rs`, with a toast rect over a view button and a divider:
  - `button_at` gives the toast's `×`, never the view button;
  - `hover_index` picks the toast button;
  - `covered` is true, so no pane, surface, tab, or divider is found;
  - `wheel_target` gives no target;
  - outside the toast, everything is as before.
- `pass_ranges`: the toast range follows top and precedes menu; empty ranges
  are skipped.
- `PendingArgs`:
  - a failed run keeps the same argument with its text and error;
  - Enter with the input unchanged returns `Retry`; editing clears it;
  - an unparseable value sets `error`.
- `command_entry`: a palette click on a command with a required arg
  (`file.rename`) gives `Prompt`, and one with none gives `Run`.
- `AppState`:
  - Save As to an unwritable folder keeps the picker open with the error, and
    a changed name then saves; Cancel closes it;
  - Open of a non-UTF-8 file keeps the picker with the error;
  - a failed Replace shows its error in the picker;
  - `file.paste` with nothing cut, run from a menu, queues an error toast
    scoped to that pane;
  - a failed copy job's report becomes an app error toast;
  - a sidebar open of a non-UTF-8 file queues a toast on that pane;
  - `autosave_all` on a read-only file queues one pane toast keyed
    `autosave:<path>`, and a second run still leaves one.
- Toast choice through `FilesView` (finding 1): a Files surface hosts two
  editor documents, and the non-active one is modified with its file made
  read-only. Its idle autosave in `tick` raises `Request::Toast` with that
  document's `DocId`. `toast_request` then `choose_toast(id, 0)` opens the
  picker with `Pick::SaveAs((sid, that doc))`, not the active document's.
- `FileBrowserView` (Harness): the error row paints in Save mode; typing
  clears it; Enter with the error showing and the name unchanged requests no
  `Pick`.
- Editor (Harness):
  - extend `diagnostics_are_underlined_and_follow_edits`: zigzag segments
    under the span, red for an error and yellow for a warning, none past the
    span; a wrapped span squiggles on each visual row;
  - Cmd+S on a read-only file raises one `Request::Toast` (error, key
    `save:<path>`), and the buffer stays modified.
- `core::config`: the report carries line, col, and severity for a clamp,
  and `read_error` for an unreadable file.
- `agent`: a user job submitted before an agent `set` stays in
  `take_file_reports` after the reply.
- Desktop check (owner, via `flag-for-owner`): colours, placement under
  strips and beside the sidebar toggle, toast over a hover popup, the picker
  error row, and dismiss on mouse-up.

## Risks

- A pointer path that skips the `pointer.rs` gate would let a click through.
  Every site listed above goes through it, and the desktop check covers the
  rest.
- A sticky toast covers the top of its pane's content (file tabs, document
  header) until dismissed. That is what the decision asks for. Tab strips stay
  clear: pane toasts sit below their own strip, and the app toast below the
  top row's strips.
- `toast_rect` lags one frame, as described under pointer handling.
- `window.rs` is 6,549 lines. The model, placement, and painting go in
  `toasts.rs`, and the pointer decisions in `pointer.rs`. The window adds
  only glue.

## Settled

1. One window-wide FIFO queue. Each toast draws at the top of its pane, or
   across the workspace (minus the sidebar) for app scope.
2. Warnings stay until dismissed, like errors.
3. Toasts are hidden under modals and menus, with their clock paused.
4. The palette's argument prompt is a form, so its errors show inline.
5. The only success toast is "Settings applied".
6. `toast.dismiss` has no default chord.
7. "Toast" goes into GLOSSARY.md (the owner used the term), and the view
   request is `Request::Toast`.
