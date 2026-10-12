# Stack review findings

Open bugs found reviewing `develop` up to `feat/editor-file-picker` (`1e1ff67`).
None is fixed anywhere in the stack. "Confirmed" means a throwaway test reproduced
it; the rest come from reading the code.

## High

### 1. Editor panics on Backspace after a click past a short line (confirmed)

- Where: `pos_at` in [`plugins/editor/mod.rs:207`](../crates/app/src/plugins/editor/mod.rs) (from `ddbb98d`).
- Repro: soft-wrap off, scroll a long line right (hscroll > the length of another
  line), click that shorter line, press Backspace.
- Cause: the unwrapped branch clamps the column to `hs.max(line_len)`, so the
  cursor lands at `hscroll` past the line's end. `EditorBuffer::backspace` then
  calls `String::remove(len)` and panics: "cannot remove a char from the end of a
  string".
- Fix: clamp to `line_len` (or clamp the cursor in the buffer's edit methods).

### 3. Config backfill corrupts a config whose section header has a trailing comment (confirmed)

- Where: `active_section_headers` / `backfill_text` in [`core/src/config.rs:471`](../crates/core/src/config.rs) (from `09c015c`).
- Repro: a config containing `[terminal] # mine`, then Cmd+, (open settings).
- Cause: header detection requires the trimmed line to end with `]`, so the
  section reads as missing and a second `[terminal]` table is appended. The file
  no longer parses, every setting falls back to its default, and backfill never
  touches it again (it skips files that don't parse).
- Fix: strip trailing comments before matching; re-parse the result and write
  only if it still parses.

## Medium

### 4. Unsaved editor content is dropped without autosave or a prompt

- Closing an editor tab/pane/workspace drops the view
  (`prune_orphan_surfaces_after`, [`app_state.rs:598`](../crates/app/src/app_state.rs)).
- Quitting (`CloseRequested`, [`window.rs:209`](../crates/app/src/window.rs))
  autosaves only when `autosave_on_unfocus = true`, and never saves pathless
  (scratch) buffers.
- TODO.md only tracks a close confirmation for multi-pane workspaces.

### 5. Save As failures are silent and still re-point the buffer

- The error goes to stderr only, after the picker has closed
  ([`window.rs:1195`](../crates/app/src/window.rs)).
- `EditorView::save_as` sets `buf.path` before writing
  ([`plugins/editor/mod.rs:527`](../crates/app/src/plugins/editor/mod.rs)), so a
  failed save leaves later autosaves aimed at the failed path.

### 6. Closing a terminal can hang the UI (not reproduced)

- `ThreadedTerminal::drop` joins the worker on the UI thread
  ([`threaded.rs:150`](../crates/terminal-ghostty/src/threaded.rs)). If the worker
  is blocked in a PTY write to a child that isn't reading (a large paste into a
  busy program), the join never returns.

## Low

7. The git filter runs `git status --porcelain` without `-z`
   ([`fs_tree.rs:446`](../crates/core/src/fs_tree.rs)): quoted paths (spaces,
   non-ASCII) never match, untracked directories are collapsed, and it runs on the
   UI thread.
8. Shell exit isn't acted on: the pane stays after `exit`. The worker refreshes
   lifecycle only when a command arrives, so a late reap can leave it `Running`.
9. Doc comments:
   - `CellAttrs::dim` carries the inverse-video comment ([`terminal/src/lib.rs:21`](../crates/terminal/src/lib.rs)).
   - `Key::Function` says F1..=F24; F13+ map to `Unidentified`.
   - `Cell::wide` is documented but never set.
   - `load_or_create`'s doc comment sits on `default_binding_ids` ([`config.rs:431`](../crates/core/src/config.rs)).

## Flaky test

`window::tests::full_screen_shaping_cost`
([`window.rs:3955`](../crates/app/src/window.rs)) asserts < 50 ms per frame; a
debug build measures ~41 ms uncontended and 100–150 ms when other builds compete
for the CPU. Raise the budget, gate it on `--release`, or `#[ignore]` it.

## To confirm

`41d9d6b` changed the first cardinal rule in `docs/GIT.md` from "do not commit
without explicit authorisation" to "committing is pre-authorised". Confirm that
is intended.
