# Stack review findings

Open bugs found reviewing `develop` up to `feat/editor-file-picker` (`1e1ff67`).
None is fixed anywhere in the stack. "Confirmed" means a throwaway test reproduced
it; the rest come from reading the code.

## Medium

### 4. Unsaved editor content is dropped without autosave or a prompt

- Closing an editor tab/pane/workspace drops the view
  (`prune_orphan_surfaces_after`, [`app_state.rs:598`](../crates/app/src/app_state.rs)).
- Quitting (`CloseRequested`, [`window.rs:209`](../crates/app/src/window.rs))
  autosaves only when `autosave_on_unfocus = true`, and never saves pathless
  (scratch) buffers.
- TODO.md only tracks a close confirmation for multi-pane workspaces.

### 5. Save As failures are silent

- The error goes to stderr only, after the picker has closed (`after_pick`,
  [`window.rs:1294`](../crates/app/src/window.rs)). The app has no
  user-visible error surface yet (no toast or status line). Decision needed:
  where should errors like this show?

## Low

7. The git filter runs `git status --untracked-files=all` on the UI thread
   (`ensure_git_status`, [`fs_tree.rs:438`](../crates/core/src/fs_tree.rs)),
   the first time a git filter or the git view is shown after a refresh. A
   large repo, or a large untracked directory (listed file by file), stalls
   a frame.
8. Shell exit isn't acted on: the pane stays after `exit`. Decision needed:
   close the pane (or tab) when its shell exits, or keep it showing the
   final output?

## To confirm

`41d9d6b` changed the first cardinal rule in `docs/GIT.md` from "do not commit
without explicit authorisation" to "committing is pre-authorised". Confirm that
is intended.
