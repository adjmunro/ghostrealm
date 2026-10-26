# Stack review findings

Open bugs found reviewing `develop` up to `feat/editor-file-picker` (`1e1ff67`).
None is fixed anywhere in the stack. "Confirmed" means a throwaway test reproduced
it; the rest come from reading the code.

## To confirm

`41d9d6b` changed the first cardinal rule in `docs/GIT.md` from "do not commit
without explicit authorisation" to "committing is pre-authorised". Confirm that
is intended.
