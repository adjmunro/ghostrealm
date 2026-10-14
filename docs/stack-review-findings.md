# Stack review findings

Open bugs found reviewing `develop` up to `feat/editor-file-picker` (`1e1ff67`).
None is fixed anywhere in the stack. "Confirmed" means a throwaway test reproduced
it; the rest come from reading the code.

## Medium

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

## To confirm

`41d9d6b` changed the first cardinal rule in `docs/GIT.md` from "do not commit
without explicit authorisation" to "committing is pre-authorised". Confirm that
is intended.
