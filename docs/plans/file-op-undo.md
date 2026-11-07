# Plan: undo and redo of file operations

`decisions-pending.md` item 14, part 3 and the file-browser half of part 4:
create, rename, move and Trash, done from the app, get an undo tree, with a
toggleable undo panel embedded in the file sidebar that shows it and walks
it. The owner wants to get back a file or folder trashed, renamed, moved or
created by mistake, and to see which surface an undo applies to. Undo across
restarts is out of scope (item 14, part 2: "No undo across restarts").

## Settled by the code today

- Every file operation runs in `AppState` (`app_state/files.rs`): `new_entry`
  (New File / New Folder), `rename_path`, a cut's Paste (`FileJob::Paste
  { cut: true }` on the file worker, applied in `apply_file_job`), and
  `trash_now` (Delete, after the git check job or a confirmation).
  Commands come from the file menu, the palette and the agent channel. The
  agent channel runs its own headless `AppState`, so its operations never
  reach the GUI's trees.
- The file browser has no multi-selection. Every operation acts on one
  entry. A folder is one entry.
- Only macOS has a Trash (`HostSystem::trash`, `NSFileManager
  trashItemAtURL`). Elsewhere Delete fails with "not supported" and never
  deletes.
- Files surfaces never change workspace. Tab drags stay inside one, and only
  web pop-ups are detached (`hidden`).

## Scope of the tree: per workspace (recommended)

| Option | For | Against |
| --- | --- | --- |
| **Per workspace** | Every op made in the workspace, from any of its Files surfaces or from the palette while it is active, is in one tree. A trashed file stays undoable after its Files surface closes, which is the main value. Matches item 3's per-workspace agent scope. | Two Files surfaces in one workspace show the same tree. Editor views of one file already share a tree the same way. |
| Per Files surface | The panel shows only what that sidebar did. | Closing the surface loses the way back to a trashed file. Palette ops with a terminal focused have no tree to go in. |
| App-wide | One list. | Cmd+Z in workspace B's sidebar undoes a rename made in workspace A, which is the ambiguity the owner wants gone. |

The tree goes when its workspace closes, as an editor's goes when its file
closes. Embedding still settles which surface an undo applies to. Cmd+Z in
the file sidebar acts on file operations, and in a document it acts on text.
The panel inside the sidebar shows exactly what that Cmd+Z will reverse.

An op is recorded in the workspace that was active when its command ran. That
is the workspace whose menu, palette or sidebar started it. A job that
finishes later (a cut's Paste, Delete's git check followed by its
confirmation) carries that workspace with it: `FileJob::Paste` and
`DeleteCheck` hold its `VtabId`, through to `Done`.

## What is recorded

A step holds one operation and its entry's identity, `Id { dev, ino, kind }`
from `symlink_metadata`, which never follows a link. `kind` is file, dir or
symlink.

| Op | Recorded | Label |
| --- | --- | --- |
| Create | path P, `Id`, and for a file its `len` (0) and `mtime` | `New file a.md`, `New folder src` |
| Rename | from P, to Q, `Id` | `Rename a.md → b.md` |
| Move (a cut's Paste) | from P, to Q, `Id` after the move | `Move a.md → docs` |
| Trash | P, the trashed location T, `Id` before trashing | `Trash a.md` |

The root row reads "Start". There is no saved step. A step whose entry
couldn't be read after it was acted on stores its `Id` (or T) as unknown, so
every later check of it refuses.

Not recorded, since no step could ever invert them:
- A move that left part of the original behind (`source_left`). Its toast
  already says so, and neither direction could be checked as one entry.
- A move across volumes (the destination's `dev` differs from the source
  folder's).
- A create where there is no Trash (`System::has_trash`), since its undo
  would trash it.
- A Trash whose location the Trash didn't report, or whose `Id` couldn't be
  read.
- A rename or paste that changed nothing (same path).
- Paste-copy and Duplicate. Open question 4 covers them.
- Any change made outside these commands.

## Safety rule

Every undo and redo checks before it acts. An error, a missing entry, a
different entry, an unreadable folder, a taken destination or an
unsupported operation means **refuse and keep**. Nothing moves, nothing goes
to the Trash, and the tree's position stays put. A toast in the pane names
the step and the reason, for example: *Can't undo "Rename a.md → b.md": a.md
already exists.* A refused restore from the Trash adds where the entry is
while the Trash still holds something there, and that Finder's Put Back can
restore it.

A step whose result is already in place counts as done, with no action: undo
of a create whose P is gone, of a rename whose P holds the `Id` and Q is
free, of a Trash whose P holds the `Id` and T is gone (Put Back by hand), and
the same for redo. So steps made moot by other means don't block the walk.

Nothing is ever deleted permanently:
- Undoing a create moves the entry to the Trash.
- Redoing a Trash moves it to the Trash again.
- Every rename and move uses a no-replace rename, so nothing is overwritten.

Inputs the checks trust, and how each can be wrong:

| Input | How it can be wrong | Guard |
| --- | --- | --- |
| `symlink_metadata(path)` | Error (EACCES from another user's folder or a sandbox). An entry replaced by another program (Finder, a shell, another workspace, an editor's atomic save). | Any error other than NotFound refuses. A different `dev`/`ino`/`kind` refuses. |
| "P is free" | Something appears at P between the check and the rename. | The rename itself is no-replace (`renamex_np RENAME_EXCL` / `renameat2 RENAME_NOREPLACE`). Undo and redo use a strict form, `rename_exclusive`, that **refuses** where the filesystem lacks it, instead of falling back to the check-then-`rename` race in `rename_noreplace`. |
| A created entry still "as created" | Written or filled since. A write lands between the check and the trash. | A file needs the same `Id`, `len` and `mtime`. A folder needs the same `Id` and `read_dir` showing no entries, and a `read_dir` error refuses. A write that races the trash still lands in the Trash, so it can be recovered. |
| Trashed location T | The Trash was emptied, Finder's Put Back ran, or another item now has that name. | T must exist with the recorded `Id`. A Trash move stays on its volume, so the inode is kept. |
| Open documents | Unsaved edits in a document of an entry about to be trashed. | `unsaved_under(path)` non-empty refuses. Redo of a Trash does not ask the way Delete does. |
| A symlink entry | Following it would act on its target. | Only `symlink_metadata` is used, and a rename moves the link itself. |
| Another OS | No Trash on Linux. | `System::trash` fails there, so undoing a create or redoing a Trash refuses and keeps. No Trash steps exist on Linux. |

### Each operation and its inverse

| Step | Undo: checks, then action | Redo: checks, then action |
| --- | --- | --- |
| Create P | P has `Id` and is unchanged (file: `len`, `mtime`; folder: empty). No unsaved document under P. Then `trash_now(P)`, which closes P's documents. | P is free. Then `create_file`/`create_dir` (`create_new` or `mkdir`, which fail if P is taken). Store the new `Id`. |
| Rename / Move P→Q | Q has `Id`. P is free. Then `rename_exclusive(Q, P)`. A case-only rename uses `same_entry_recased` and a plain rename, as `file_ops::rename` does. Documents follow (`changed(&[(Q, P)])`). | The same with P and Q swapped. |
| Trash P→T | T has `Id`. P is free. Then `rename_exclusive(T, P)`. A different device (EXDEV) refuses. Documents closed by the Trash stay closed. | P has `Id`. No unsaved document under P. Then `trash_now(P)`. Store the new T. |

A missing or non-folder parent of P needs no check of its own: `stat(P)`
fails with ENOTDIR or EACCES (refuse), or the atomic rename or create fails
with ENOENT (refuse).

Undo and redo call `trash_now`, which returns T and the `Id`; only Delete's
callers record a Trash step, so undo and redo never record one.

Undo and redo of a move across filesystems refuse: the rename returns EXDEV,
and the toast says it was moved across volumes and must be moved back by
hand. The worker-job alternative is open question 5. With this choice, every
undo and redo is one synchronous rename or trash on the UI thread.

Operations by other means (Finder, a shell, another workspace, an editor)
aren't tracked. Their effect shows up only as a failed check when a step is
taken. Rows are not pre-checked or greyed, which would need a `stat` per row
per paint. A refused step stays in the tree and works again if its
preconditions return. Steps do not follow moves made by other means or by
other workspaces' trees. Within one tree the order of undo and redo replays
the states each step was recorded in, so paths stay consistent.

### Walking more than one step

Clicking a far row, or Home or End, walks the route from the current step to
the target: undo up to the common ancestor, then redo down. That is the
editor's `route`, taken one step at a time. Each step is checked and acted on
before the next. The first refusal stops the walk at the last step reached,
and the toast says which step stopped it.

There is no confirmation, as in the editor. Every inverse is a no-replace
rename or a move to the Trash, so no walk can lose data.

### Restoring one Trash step

A Trash row in effect offers a Restore button in the panel. It puts that
entry back on its own, out of order, with the undo's checks, and marks the
row "restored"; the current step stays. Undoing it later finds the entry
already in place (done); redoing it trashes it again and clears the mark.

Multi-selection (later): one command over N entries becomes one step of N
operations, all checked before any acts.

### Async jobs

- Undo, redo and restore refuse while any file job is pending
  (`!file_jobs_idle()`): "Can't undo while a file operation is running: wait
  for it to finish." A running copy or move could be using the paths a step
  checks, and a move's step is added only when its job finishes.
- A cut's Paste records its step in `apply_file_job` when it finishes, as a
  child of the tree's current step at that moment. Undo can't run meanwhile,
  so that is the step that was current when it started, or a step recorded
  synchronously after it.
- Delete records its Trash step in `delete_now`, when the item really moves.
  That happens after the git check job and any confirmation. The workspace
  goes with it: `FileJob::DeleteCheck` carries the `VtabId`, and
  `Confirmed::Trash`, `UnsavedTrash` and `Resume::Trash` carry it on.
- A job that finishes after its workspace closed records nothing. Its toast
  still reports the outcome.

## Trash on each OS

**macOS (run-tested here).** `System::trash` returns
`Result<Option<PathBuf>>`, the trashed location from `trashItemAtURL
:resultingItemURL:error:` (objc2-foundation 0.3:
`Option<&mut Option<Retained<NSURL>>>`). A restore is
`rename_exclusive(T, P)`. Without a reported T, the Trash step is not
recorded.

TCC protects `~/.Trash`: listing it needs Full Disk Access. Renaming a known
item out of it does not. The `#[ignore]`d test `real_trash_round_trip`
trashes a temp file through `HostSystem` and restores it; it passed when run
by hand from a process without Full Disk Access (`ls ~/.Trash` refused).

**Linux (compile-checked only).** There is no Trash: `HostSystem::trash`
keeps failing, with only the signature changed, and `has_trash` is false. So
no Trash or create step is ever recorded there. The plan adds no Linux
Trash or restore code. That is destructive work on an OS this machine can't
run. Following the freedesktop Trash spec, a restore would rename
`$XDG_DATA_HOME/Trash/files/<name>` (or `$topdir/.Trash-$uid/files/<name>`)
exclusively back to the `Path=` in `info/<name>.trashinfo`, then remove that
`.trashinfo` only after the rename succeeds. That goes in the existing TODO
bullet "Delete on other platforms". The non-macOS `cfg` changes are
compile-checked with the build skill's scratchpad `target_os` swap. Rename
and move undo use `cfg(unix)` code shared with macOS and are run-tested here,
except the Linux `renameat2` branch of `rename_exclusive`, which is
compile-checked only.

## The tree

New `crates/app/src/file_history.rs`, about 200 lines of code and 150 of
tests:
- `FileHistory`: nodes `{ parent, children, redo, op: FileOp }`, plus root,
  current and the next id. Methods:
  - `record(op)`: add a child of current, set the redo link, make it current.
  - `route(target) -> Vec<Move>`: `Undo(id)` steps up to the common
    ancestor, then `Redo(id)` steps down.
  - `undone(id)` / `redone(id)`: move current one step and set the parent's
    redo link.
  - `op_mut(id)`: update a step's recorded identity or location after it is
    acted on.
- `FileOp`, with its `Id`, its checks (pure functions over the filesystem,
  testable without `AppState`) and its label.
- No step cap. Operations are paced by a person, and a step is a few hundred
  bytes.

The editor's `UndoTree` is not made generic. Its nodes carry text changes,
carets, staleness and byte-capped pruning, and file history needs none of
those. Sharing the core would mean reworking code that has just been
reviewed, to save about 60 lines of parent/child bookkeeping.

`AppState` keeps `file_histories: HashMap<VtabId, Rc<RefCell<FileHistory>>>`,
created on first use and removed in the workspace's close path. `insert_view`
is where every surface in a pane gets its view. When that view is a
`FilesView`, it gets its workspace's handle. The sidebar's panel borrows the
handle only while painting. `AppState` mutates it only while handling
requests, when no view holds a borrow, so the editor's `history()` `Ref`
hazard (TODO "Editor follow-ups (g)") can't happen here.

Undo, redo and walking go in a new `app_state/file_undo.rs`, which keeps
`files.rs` from growing:
- `walk_file_history(sid, Walk::{Undo, Redo, To(step), Restore(step)})`
  resolves `sid`'s workspace and runs the checks and actions above.
- It reuses `trash_now`, `changed`, `unsaved_under` and `system`.
- A single undo or redo shows an info toast naming the step ("Undid rename
  a.md → b.md"), unless the sidebar's panel is open and shows it.

## Sharing the panel

`plugins/editor/undo_panel.rs` moves to `crate::plugin::undo_panel` and
becomes generic over a trait:

```rust
pub trait History {
    fn root(&self) -> StepId;
    fn current(&self) -> StepId;
    fn saved(&self) -> Option<StepId>;
    fn steps(&self) -> usize;
    fn parent(&self, id: StepId) -> Option<StepId>;
    fn children(&self, id: StepId) -> &[StepId];
    fn label(&self, id: StepId) -> String;
    fn note(&self, _id: StepId) -> Option<&'static str> { None }
    fn action(&self, _id: StepId) -> Option<&'static str> { None }
}
```

`note` tags a row ("restored"); `action` labels a button on it ("Restore").
`paint` takes `PanelIds { rows, close, actions }` and the `Edge` it borders
its host on (a hairline): left in the editor, top in the sidebar.

- `StepId` (now the editor's `NodeId`) moves with it. `history.rs` imports it
  as `NodeId`, which keeps the diff small.
- `Graph::of`, the graph key, `UndoPanel` (`toggle`, `focus`, `key`,
  `scroll`, `step`, `paint`) stay in the shared module. The lane tests stay
  on `UndoTree` in the editor module.
- The editor keeps what is its own in `plugins/editor/undo.rs`:
  `impl History for UndoTree`, plus `label`, `merged` and `snippet`, their
  tests, the panel's editor tests, and the panel's width (`PANEL_CELLS`,
  half the body). `paint` takes the area its host gives it.
- This is one topological commit with no behaviour change, and every
  existing test passes unchanged in content.

The file sidebar draws the panel at its **bottom**, the full sidebar width,
`min(14 rows, half the sidebar's height)`. The sidebar is too narrow to share
sideways. It shows only in `Mode::Sidebar`, not in the pickers, and hides
with the sidebar. Row `i` is button `BTN_UNDO + i` and its Restore button
`BTN_RESTORE + i`, past every other file-browser id and below the ids a
Files surface keeps for its document. The close button is `BTN_UNDO_PANEL`,
which is also the `⋯` menu's "Undo history" check item, as in the editor. A
press on the panel gives it the keys; a click on the list or the filter
takes them back.

## Keys

| Where | Key | Does |
| --- | --- | --- |
| File sidebar has the keys, filter **empty** with nothing of its own to undo | Cmd+Z (Ctrl+Z) | Undo the workspace's last file operation |
| same, nothing of its own to redo | Cmd+Shift+Z (Ctrl+Y, Ctrl+Shift+Z) | Redo |
| File sidebar, filter has text or its own history | those chords | Undo or redo the filter's text, as today (`LineEdit`) |
| Sidebar undo panel has the keys | Up/Down, Page Up/Down, Home/End | Walk the steps, as the editor's panel does |
| same | Cmd+Z / Cmd+Shift+Z | File undo and redo, whatever the filter holds |
| same | Escape, Enter, typing, Shift or Option keys | Give the keys back to the filter. A typed key filters. |
| Anywhere | `cmd+shift+u` | Toggle the undo panel of the part with the keys (see below) |

The view's key handling sends `Request::FileHistory(Walk)`. That is applied
in `window.rs` `apply_outcome` and in `send_key_to_focused` (headless).

**The toggle command.** `editor.toggle_undo_history` and its `cmd+shift+u`
stay. While the focused Files surface's sidebar has the keys it toggles the
sidebar's panel, and otherwise the focused editor's.

## Removed or replaced

- `plugins/editor/undo_panel.rs` moves to `plugin/undo_panel.rs`, and its
  editor-only parts go to `plugins/editor/undo.rs`.
- `System::trash -> Result<()>` becomes `-> Result<Option<PathBuf>>`.
- `FakeSystem::trash` no longer removes the entry. It moves it into a
  `trash_dir` the test gives it (a unique name inside it) and returns that
  path. Existing tests still see the entry leave its place.
- `rename_noreplace` splits so that its strict half, `rename_exclusive`, can
  be called on its own. Existing callers keep the fallback.
- `trash_now(path) -> Result<()>` becomes `-> Result<Option<(PathBuf, Id)>>`
  (T and the `Id`); Delete's callers go through `delete_now`, which records.

## Tests

All run on macOS here with `FakeSystem` in tmpdirs, unless marked otherwise.
Each guard test drives the check to fail and asserts the file is kept, with
its contents and in its place, that `FakeSystem` recorded no trash, and that
the tree's current step is unchanged.

`file_history.rs` (tree and checks):
- `record` and undo/redo links; `route` across branches through the common
  ancestor; a new op after an undo branches and keeps the old branch; labels.

`app_state/file_undo.rs`, the happy paths:
- create → undo (trashed, document closed) → redo (created again, new `Id`)
- rename → undo → redo, with an open document following
- a case-only rename → undo → redo (APFS)
- move (cut's Paste) → undo → redo
- Trash → undo (restored from the fake Trash) → redo (trashed again, new T)
- a walk across a branch
- steps already in effect (a create gone, a Trash put back by hand) are taken
  without acting
- a Trash restored on its own leaves the steps after it; a restore refuses
  like an undo

`app_state/file_undo.rs`, one test per guard:
1. Undo create file: written since (`len` > 0) → kept.
2. Undo create file: replaced by another empty file (other inode) → kept.
3. Undo create folder: holds an entry → kept with its entry.
4. Undo create folder: unreadable (`chmod 000`) → kept.
5. Undo create: open document with unsaved edits → kept, document still open.
6. Undo create when the Trash fails (its folder read-only) → kept.
7. Undo rename: P taken by a new file → both kept with their contents.
8. Undo rename: Q's entry replaced (other inode) → both kept.
9. Undo move: P's parent gone, or a file → Q kept.
10. Undo rename: P's parent unreadable (`stat` EACCES, not NotFound) → kept.
11. Undo move while a job is in flight (`FileJob::Hold`) → nothing moves.
12. Undo Trash: T gone from the fake Trash → P stays free, nothing created.
13. Undo Trash: P taken → T stays in the Trash, and P keeps its contents.
14. Undo Trash: T replaced (other inode) → both kept.
15. Redo Trash: P's entry replaced → kept, not trashed.
16. Redo Trash: unsaved document under P → kept.
17. Redo create: P taken → the existing entry kept, untouched.
18. Redo rename/move: Q taken → both kept.
19. A walk whose third step fails stops at the second, and later entries are
    untouched.
20. A symlink entry: undoing its rename moves the link, its target is
    untouched, and `Id` is the link's.

Scope, keys and panel:
- An op in workspace A is in A's tree, not B's.
- Closing a workspace drops its tree, and a job finishing after it records
  nothing.
- A Trash confirmed after switching workspace records in the workspace the
  Delete started in.
- A rename or paste that changes nothing, and a create with no Trash, record
  nothing.
- `rename_exclusive` refuses an existing file and an existing empty folder.
- `file_browser` (Harness):
  - Cmd+Z with an empty filter requests file undo; with text or the filter's
    own history it edits the filter.
  - The panel toggles from `⋯`, shows a row per step at the sidebar's
    bottom, its rows and arrows request `To`, and a Trash row's Restore
    requests `Restore`.
  - Escape, or a click on the list or the filter, gives the keys back.
- `editor.toggle_undo_history` picks the sidebar's panel when the sidebar has
  the keys, and the editor's otherwise.
- A walk toasts only while the panel is closed.
- `#[ignore]` `real_trash_round_trip` (macOS, `HostSystem`). Run once by
  hand, as described above.

No test can make `rename_exclusive` meet a filesystem without no-replace
renames. Its refusal is one `match` arm, covered by review.

Commands:
- `cargo test --workspace` with `GHOSTREALM_NO_WEB=1`.
- `cargo clippy`.
- The non-macOS scratchpad compile check (build skill).

## Docs

- `DECISION.md`: one entry. File-operation undo per workspace, the checks
  and the refusal rule, undoing a create means the Trash, cross-volume moves
  refuse, and the sidebar panel and its keys.
- `PLUGINS.md`: the undo panel section, with its new module and the sidebar's
  use.
- `README.md`: the keys table.
- `decisions-pending.md` item 14: the "Built" line.
- `TODO.md`:
  - Drop "Undo / Redo of file operations" and the undo entry's REMAINING
    sentence.
  - Add Linux restore to "Delete on other platforms".
  - Update the paths in the undo panel follow-ups.
  - Record the follow-ups: stale paths from sync ops during a paste, inode
    reuse, case-only renames on case-sensitive volumes, refusing only
    overlapping paths while a job runs, `.DS_Store` in a new folder, agents'
    ops once item 3 is built, and why SMB/exFAT/FUSE refuse.

## Commits (branch `feat/file-op-undo`)

1. `refactor(undo): share the undo panel through a History trait` (topological)
2. `feat(files): record file operations in a tree per workspace`, including
   `System::trash` returning the trashed location
3. `feat(files): undo and redo file operations from the file sidebar`
4. `feat(files): show the file history in an undo panel in the file sidebar`,
   including the panel's `Edge` and the toggle command's sidebar case
5. `docs: file operation undo`

## Size

About 2,100 changed lines of Rust, not counting the panel's move:

| Part | Lines |
| --- | --- |
| Panel move, trait, notes and actions | about 160 changed, the rest moved |
| `file_history.rs` | 335 |
| `file_undo.rs` | 210 |
| Recording and workspace plumbing in `files.rs`, `app_state.rs`, `file_jobs.rs` | 140 |
| `file_ops.rs` (`Id`, `rename_exclusive`, trash location) | 90 |
| File-browser panel, keys and requests | 110 |
| Command | 10 |
| **Code total** | **about 1,050** |
| Tests | about 1,000 |
| Docs | about 120 |

## Open questions (recommended answer first)

1. **Tree scope.** Per workspace. Per Files surface or app-wide are the
   alternatives (see the table above).
2. **Toggle chord.** Settled by the owner: `editor.toggle_undo_history` and
   `cmd+shift+u` stay, and toggle the sidebar's panel while it has the keys.
3. **Cmd+Z in the sidebar.** Settled by the owner: file undo while the filter
   is empty and has nothing of its own to undo, filter text otherwise. The alternative gives file undo only while the panel
   has the keys: stricter, and one more step for every undo.
4. **Paste-copy and Duplicate as undoable creates.** No. Adding them is a
   feature: undo would trash the copy only if no file in it changed, which
   needs a stamp per copied file.
5. **Undo of a move across volumes.** Refuse. Supporting it is a feature: a
   worker job copying back to the exact path and removing what it copied,
   with undo then async.
6. **Rename/move identity.** The inode must match. Another editor's atomic
   save swaps the inode and makes the undo refuse. Accepting any entry of the
   same kind would relax the brief's "still holds what the op left".
7. **Undo/Redo items in the file context menu** (Zed's Edit section). No.
   Adding them is a feature; the chords and the panel cover the need.
8. **macOS restore if TCC refuses renames out of `~/.Trash`.** Settled: the
   hand-run test showed TCC allows it without Full Disk Access.
9. **Reopen documents a Trash closed when the Trash is undone.** No. Reopening
   them is a feature.
10. **Linux Trash and restore.** They stay off, with a TODO entry, until they
    can be run on Linux.
