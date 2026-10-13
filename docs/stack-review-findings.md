# Stack review findings

Open bugs found reviewing `develop` up to `feat/editor-file-picker` (`1e1ff67`).
None is fixed anywhere in the stack. "Confirmed" means a throwaway test reproduced
it; the rest come from reading the code.

## Medium

### 4. Unsaved editor content is dropped without autosave or a prompt

- Closing an editor tab/pane/workspace drops the view
  (`prune_orphan_surfaces_after`, [`app_state.rs:685`](../crates/app/src/app_state.rs)).
- Quitting (`CloseRequested`, [`window.rs:220`](../crates/app/src/window.rs))
  autosaves only when `autosave_on_unfocus = true`, and never saves pathless
  (scratch) buffers.
- TODO.md also tracks a close confirmation for multi-pane workspaces.
- Tracked in TODO.md ("Quit/close confirmation for unsaved editor content"):
  a Save and Quit / Cancel / Quit Without Saving dialog; opening a popup, menu,
  or dialog does not count as losing focus for autosave.

### 5. Save As and open failures are silent

- A Save As error goes to stderr only, after the picker has closed
  (`after_pick`, [`window.rs:1318`](../crates/app/src/window.rs)).
- So does a file that won't open (not UTF-8, unreadable): clicking it in the
  file browser or opening the settings does nothing visible
  (`open_requested_file`, [`window.rs:1010`](../crates/app/src/window.rs);
  `open_config_editor`, `:838`).
- The app has no user-visible error surface yet (no toast or status line).
  Tracked in TODO.md ("User-visible error surface"): it needs a design, and the
  user wants to be prompted about it.

## Low

7. The git filter runs `git status --untracked-files=all` on the UI thread
   (`ensure_git_status`, [`fs_tree.rs:438`](../crates/core/src/fs_tree.rs)),
   the first time a git filter or the git view is shown after a refresh. A
   large repo, or a large untracked directory (listed file by file), stalls
   a frame.
8. Shell exit isn't acted on: the pane stays after `exit`. Decision needed:
   close the pane (or tab) when its shell exits, or keep it showing the
   final output?
   **Decided:** close the pane on exit code 0. On a non-zero exit or a signal,
   keep the pane with a banner ("exited with code N — press any key to close");
   a setting overrides this. Tracked in TODO.md.

## To confirm

`41d9d6b` changed the first cardinal rule in `docs/GIT.md` from "do not commit
without explicit authorisation" to "committing is pre-authorised". Confirm that
is intended.
