# Plan: the agent channel in the running app, scoped per workspace

`decisions-pending.md` item 3, first increment. An agent in a terminal tab
drives the window it runs in, limited to its own workspace, and can't reach
the person's web pages. The app enforces the limit: a token bound to the
workspace, checked at one place every request passes through, and a default
of refusal for anything not classified.

## Settled by the code today

- `ghostrealm agent` (`agent.rs`) builds its own `AppState` and serves
  `help`, `search`, `set`, `get tree`, `get surface`, `input` and `pump`
  over stdin/stdout. `handle` parses and runs a request in one `match`.
- Commands act on "the active workspace": `AppState::active()` (the tree's
  active vtab), `focused_pane()` and `focused_surface()` derive from it.
  Only `active()` and `pump_all_budgeted` read `tree.active_vtab()` in
  `AppState`.
- A surface never changes workspace: tab drags move within one vtab
  (`Tree::move_surface` takes the vtab). Web pop-ups an agent opened are
  `hidden` surfaces, in no workspace.
- Vtab ids come from a counter and are never reused.
- A web view is the agent's only when opened without a waker
  (`WebView::agent = cx.waker.is_none()`), i.e. only in `ghostrealm agent`.
  In the window every page is on the one person's profile.
- Terminals spawn through `TerminalPlugin::open(&OpenCx)`; `CommandBuilder`
  sets extra env on top of the app's own and can't remove a variable.
- The window wakes its loop through `EventLoopProxy<UserEvent>`; `AppState`
  is `!Send` (`Rc`, `RefCell`), so every request must run on the UI thread.

## Socket

- Directory: `$XDG_RUNTIME_DIR/ghostrealm-<uid>` when `XDG_RUNTIME_DIR` is set
  and absolute, else `std::env::temp_dir()/ghostrealm-<uid>` (macOS: the
  per-user `$TMPDIR`; Linux without a runtime dir: `/tmp`). Created with
  mode 0700. If it exists, `lstat` must show a directory (not a symlink)
  owned by our uid with no group or other bits; otherwise the window serves
  no socket. Never `chmod`ed or removed. Symlinks above it are allowed
  (macOS `/var` → `/private/var`).
- File: `<pid>-<8 random hex>.sock`, so no existing path is ever reused or
  removed before binding. A path longer than `sun_path` (104 bytes on macOS)
  fails the bind.
- One socket per window, owned by the window's `State`. The app has one
  window; a second would bind its own.
- When the socket can't be made, shells get no agent env and a warning
  toast says why ("Agent channel unavailable: …").
- Window close: dropping the server stops the accept thread (it wakes it by
  connecting to itself), drops every connection's reply channel (each
  connection thread then closes its stream), and removes the socket file
  under the safety rule below. A crash leaves the file; a client then gets
  "connection refused". Stale files are left to the OS (`$TMPDIR` cleanup,
  `XDG_RUNTIME_DIR` on logout).

### Safety rule for the one deletion

The change deletes only the socket file it bound. Every error, mismatch or
unreadable state means keep.

| Input the deletion trusts | How it can be wrong | Guard | Test |
| --- | --- | --- | --- |
| The path we bound | Replaced since (another process, a person) | Remove only when `lstat`'s `(dev, ino)` equals what `lstat` gave right after bind | Replace the socket with a regular file, drop the server: the file is kept |
| `lstat` | Error (gone, permission, sandbox) | Error → keep | Remove the directory's search bit before drop: nothing removed, no panic |
| The path's last component | A symlink put there | `lstat` doesn't follow; its inode differs → keep | Replace the socket with a symlink to a file outside the dir: both kept, target unchanged |
| The directory | Pre-made by another user, a symlink, or loose mode | Checked before bind; refused → no socket made, nothing removed | Pre-make the dir as a symlink, and with mode 0755: no socket, the symlink's target and the 0755 dir unchanged |
| Owner | Another uid | Checked before bind | Read only: a test can't make a file owned by another user without root |
| Another OS | Linux | Same `std::os::unix` code; no `cfg` | Compile-checked per the build skill |

A same-user process swapping the file between `lstat` and `unlink` defeats
the inode check; such a process can delete our files anyway.

## Tokens

- 32 bytes from `/dev/urandom` (no new dependency), hex-encoded. A failed
  read leaves that workspace without a token: its shells get no agent env;
  stderr says why.
- `AppState` holds `HashMap<VtabId, String>` and the socket path, set by the
  window once bound. A workspace's token is minted the first time a terminal
  opens in it.
- `open_cx(cwd)` becomes `open_cx(vt, cwd)`, with `vt` the workspace of the
  surface being filled (`vtab_of_surface(sid)`). `OpenCx` gains
  `agent_env: Option<(PathBuf, String)>`. The terminal plugin sets
  `GHOSTREALM_AGENT_SOCKET` and `GHOSTREALM_AGENT_TOKEN` from it, and
  removes both when it is `None`, so a shell never inherits them from the
  app's own environment (an app started from another window's shell, or a
  test run from one). `CommandBuilder` gains `env_remove`.
- Binding: a token resolves to its workspace only while that workspace is in
  the tree. Every request resolves it again, so closing a workspace by any
  path (Close Workspace, its last pane, a confirmation answered later)
  revokes its token with no hook; the stale entry is pruned on that lookup.
  Ids aren't reused, so an old token can't name a new workspace.
- Comparison: every entry is compared in constant time (few workspaces).
- Window close: tokens die with the process; the shells are hung up.

## Protocol

- The first line on a connection is `{"op":"hello","token":"…"}`. Reply
  `{"ok":true,"workspace":<id>}`, or `{"ok":false,"error":"token not valid
  (unknown, or its workspace closed)"}` and the connection is closed. Any
  other first line gets the same refusal.
- Every later line is a request as today. The connection thread attaches the
  token it got in `hello`; the UI thread resolves it per request and, once it
  no longer resolves, replies with the refusal and closes.
- `ghostrealm agent` sends `hello` itself, so an agent sees the same
  protocol live and headless.

## The single enforcement point

`agent::serve(line, scope, state, registry) -> Reply`, used by both the
headless loop (`Scope::Full`) and the socket (`Scope::Workspace(vt)`).
Inside: parse → authorize → run.

- Parse: the line becomes an `Op` enum (`Help`, `Search`, `Set`, `GetTree`,
  `GetSurface`, `Input`, `Pump`). An unknown op is an error before anything
  runs.
- Authorize: one `match` over `Op` with no wildcard arm, so a new variant
  doesn't compile until it is classified. Under `Scope::Full` everything
  passes (headless: nobody else is there).

| Op | Under `Scope::Workspace(vt)` |
| --- | --- |
| `help`, `search` | App-wide. List only commands the scope may run. |
| `pump` | App-wide. |
| `get tree` | Only `vt`, only reachable surfaces; no hidden surfaces. |
| `get surface` | `id` must be reachable (below); otherwise `unknown surface <id>`, the same reply as a surface that doesn't exist. |
| `input` | `id` must be reachable; with no `id`, `vt`'s focused surface, which must be reachable. |
| `set` | By the command's class (below). |

- Reachable surface: in `vt`'s panes (`vtab_of_surface(sid) == Some(vt)`)
  and its view says `View::agent_reachable()`. The hook defaults to `false`;
  `TerminalView` and `FilesView` return `true`. `WebView` keeps the
  default: every page in the window is on the person's profile. A new
  surface kind is unreachable until its view opts in.
- Command class: `CommandMeta` gains `agent: AgentReach`, `Refused` by
  default in `CommandMeta::new`, set with `.agent_workspace()` or
  `.agent_focused_pane()` beside the command's registration. A command
  nobody classified is refused and left out of `help` and `search`.
  - `Workspace`: runs with `vt` as the acting workspace (below). An `id`
    argument, where a command has one, must be reachable.
  - `FocusedPane`: as `Workspace`, and refused unless every surface in
    `vt`'s focused pane is reachable or empty, since the command closes the
    focused tab or pane.
- Acting workspace: `AppState` gains `acting: Option<VtabId>`, and `active()`
  returns it before the tree's active vtab. `AppState::acting_as(vt, f)` sets
  it for the command's run and restores it. The person's active workspace
  doesn't change: `focused_pane()`, `focused_surface()`, `files_target()`
  and `resolve_cwd` resolve against `vt`. `get` ops don't run under it:
  `adopt_popups` places orphaned web tabs in the focused pane and must keep
  using the person's.

### Classification of today's commands

`Workspace` (30): `workspace.rename`, `workspace.mark_read`,
`workspace.mark_unread`, `workspace.needs_input`, `workspace.dismiss`,
`split.leftright`, `split.topbottom`, `pane.focus_next`, `pane.focus_prev`,
`surface.new`, `terminal.new`, `file_browser.new`, `editor.new`,
`editor.open`, `editor.toggle_comment`, `editor.cycle_view_mode`,
`editor.toggle_undo_history`, `files.focus_sidebar`, `files.next_tab`,
`files.prev_tab`, `file.open`, `file.show_in_browser`,
`file.open_in_terminal`, `file.duplicate`, `file.delete`, `file.new_file`,
`file.new_folder`, `file.rename`, `file.add_to_gitignore`,
`file.add_to_git_exclude`.

`FocusedPane` (2): `pane.close`, `pane.close_all`.

`Refused` (21), each with its reason:

| Commands | Why |
| --- | --- |
| `web.new`, `web.navigate`, `web.back`, `web.forward`, `web.reload`, `web.close`, `web.focus_url`, `web.eval` | Every page in the window is on the person's profile. |
| `workspace.new` | Reaches outside the workspace (the standing grant, later) and makes the new one active. |
| `workspace.close` | Closes every surface in it, a person's web pages included. The agent's shell can `exit`. |
| `workspace.pick_dir`, `editor.save_as`, `file.save_copy_as` | Open the picker, a modal for the person. |
| `toast.dismiss` | Dismisses what the person is reading. |
| `file.cut`, `file.copy`, `file.paste` | The person's file clipboard. |
| `file.copy_path`, `file.copy_relative_path` | The person's system clipboard. |
| `file.reveal`, `file.open_default` | Launch Finder or another app on the person's desktop. |

`palette.toggle` is the window's, not in `build_registry`; unaffected.

No command is app-wide today, so `AgentReach` has no `App` variant; the
first app-wide command adds it.

### What a scoped command can still do

- File commands take paths anywhere; the shell already can.
- `file.rename` of a folder above another workspace's root moves that root,
  as it does from the palette.
- A command that asks (Close with unsaved changes; Delete of an untracked
  file) shows its confirmation to the person, who answers it.
- With the person in the same workspace, the agent's focus changes happen
  under their hands.

## Web surfaces are excluded structurally

- No path reaches a page: `WebView::agent_reachable()` is `false`, so the
  surface is absent from `get tree`, `get surface` answers as for a missing
  id, `input` is refused; all `web.*` commands are `Refused`, absent from
  `help`; `pane.close`/`pane.close_all` refuse a pane holding one;
  `workspace.close` is refused.
- Hidden surfaces belong to no workspace, so the workspace check refuses them
  before the view check.
- Not covered: the person's profile on disk (`~/Library/Caches/ghostrealm/web`)
  is readable by any process running as the person, an agent's shell
  included. That is item 3's cookie-encryption follow-up, out of scope here.

## Threading

- Accept thread: `UnixListener::incoming()`. At most 32 live connections;
  the 33rd gets `{"ok":false,"error":"too many connections"}` and is closed.
- One thread per connection, one request in flight: read a line (capped at
  1 MiB; over it: an error reply and close), send `Incoming { token, line,
  reply: Sender<Reply> }` on an `mpsc` channel, call the waker, block on its
  own reply receiver, write the reply (write timeout 10 s; on timeout, close),
  read the next line. A client that stalls mid-line or stops reading blocks
  only its own thread.
- The waker is an `Arc<dyn Fn() + Send + Sync>` sending
  `UserEvent::Agent` through the proxy (the PTY waker's pattern), so the
  server has no winit type and tests drive it without a window.
- UI thread: `user_event(Agent)` drains the channel with `try_recv`, runs
  `agent::serve` on `state.app` with the window's registry, sends each reply
  (a non-blocking `mpsc` send) and marks a frame due. No socket I/O happens
  on it.
- File commands: headless keeps waiting for the jobs before replying. Live,
  `set` that started file jobs parks its reply with the job id range
  `[mark, end)`. Each pass of `about_to_wait`, before
  `toast_file_reports`, the parked replies take the reports in their range
  (so the window doesn't toast them) and reply once all are in. A large
  copy never blocks the UI.
- Live, exited terminals follow the person's `close_on_exit`: the headless
  `keep_exited` override stays headless.

## Client

`ghostrealm agent` with `GHOSTREALM_AGENT_SOCKET` set connects to it, sends
`hello` with `GHOSTREALM_AGENT_TOKEN` (empty when unset, so refused), then
relays stdin lines to the socket and reply lines to stdout until either
side closes. A refused `hello`, a socket that won't connect (window closed,
a crash's stale file), or extra command-line arguments print an error to
stderr and exit 1; the error for a dead socket says to unset
`GHOSTREALM_AGENT_SOCKET` to run headless. With the variable unset it runs
headless as today. No silent fallback to headless: the agent would drive a
copy believing it drives the window.

The env names a socket any client can speak to (`nc -U`, a script), so
`ghostrealm` need not be on the shell's `PATH`.

## What a same-user process outside the workspace can do

| | macOS | Linux |
| --- | --- | --- |
| Connect to the socket | Yes (the 0700 dir admits our uid and root) | Yes |
| Anything without a token | No: refused, disconnected | No |
| Fill the 32 connection slots | Yes: denies the agent channel, not the UI | Yes |
| Read a shell's token from its environment | Expected no (`KERN_PROCARGS2` is restricted); unverified: from this session's sandbox `ps -wwE` showed no environment for a same-user child, which the sandbox may explain. Needs a check at a desktop. | Yes: `/proc/<pid>/environ` needs only `PTRACE_MODE_READ`, which Yama's `ptrace_scope` doesn't restrict. Such a process gets that workspace's reach. |
| Read the app's memory (all tokens) | Only with debugger rights (`task_for_pid`: Developer Tools access and a binary without the hardened runtime, or root) | With `ptrace` attach: by default (Yama 1) only an ancestor or root |
| Inherit a token | Any descendant of a workspace's shell, including through a tmux server started there and attached from another workspace | Same |

So on Linux the workspace boundary holds against an agent that doesn't go
looking, not against a same-user process that does. Binding the connection
to the client's process ancestry (`SO_PEERCRED` / `LOCAL_PEERPID`, then the
parent chain up to a workspace shell) would close that; it is listed under
"Later".

## Removed or replaced

- `agent::handle`: replaced by `agent::serve` (parse → authorize → run). Its
  duties all move there; headless passes `Scope::Full`.
- `AppState::open_cx(cwd)`: becomes `open_cx(vt, cwd)`; the extra argument
  feeds the agent env.
- `run_cli`'s `agent` branch: picks live or headless by the environment.
- Nothing else is removed. No script or path is replaced.

## Tests

Socket and protocol (`agent/socket.rs`, a test thread acting as the UI
thread over an `AppState` with two workspaces A and B):

- No token: a first line that isn't `hello` → refusal, then EOF.
- Wrong token: `hello` with a random token → refusal, then EOF.
- A's token aimed at B: `get surface` of B's terminal → `unknown surface`;
  `input` to it → refused, and B's shell received nothing; `get tree` lists
  A only.
- Web surface: a `WebView` (with a waker, as the window makes them) in A:
  absent from `get tree`; `get surface` and `input` refused; `web.*` absent
  from `help` and `search`; `set web.new` and `set web.eval` refused;
  `pane.close` with it focused refused and it stays.
- Closed workspace's token: close A; the open connection's next request →
  refusal and EOF; a new `hello` with that token → refusal.
- Stalled client: client 1 sends half a line and stops; client 2 connects,
  says `hello`, gets a `get tree` reply within 2 s. Client 3 sends requests
  and never reads; client 2 still gets replies. The UI loop's `try_recv`
  drain returns throughout.
- Over-long line → error reply and close. 33rd connection → refused.
- File command live: a copy's reply arrives once its job is done and the
  window's report queue doesn't hold it.
- Safety rule: one test per row of the table above.

Enforcement (`agent.rs` unit tests, no socket):

- A command registered without a class is refused under a workspace scope
  and runs under `Scope::Full`.
- For every command classified `Workspace` or `FocusedPane`, run with
  sensible arguments (a temp dir for paths) scoped to background workspace
  A while B is active: B stays active, and B's panes, surfaces and titles
  are unchanged.
- `input` with no `id` goes to A's focused surface, not B's.
- `help` under a scope lists exactly the `Workspace` and `FocusedPane`
  commands.

Tokens and env:

- A terminal opened in a workspace of an `AppState` with a socket path sees
  both variables (`printf "$GHOSTREALM_AGENT_TOKEN"` in its text); one in
  another workspace sees a different token.
- `env_remove`: the built `Command`'s `get_envs()` lists both variables as
  removed (`None`) when the terminal has no agent env, so a value in the
  app's own environment never reaches the shell. Inspecting the `Command`
  avoids `std::env::set_var`, which races other tests.

Client:

- `ghostrealm agent` (the built binary) with the variables pointing at a
  test server relays `help` and its reply; pointing at a missing socket
  exits 1 with the message.

Existing headless tests in `agent.rs` keep passing under `Scope::Full`.

## OS-specific code

None behind `cfg`. The socket, directory checks and client use
`std::os::unix` and `libc` (`getuid`, already a dependency) on both. Run on
macOS; Linux compile-checked per the build skill.

## Docs

- `DECISION.md`: a new entry: the agent channel runs in the window over a
  per-window socket in a 0700 directory; per-workspace tokens in shells'
  environment; one enforcement point; commands and surface kinds refused
  until classified; person-profile pages unreachable; the Linux
  `/proc/<pid>/environ` gap accepted for now.
- `SPEC.md`: Status phase 3 line and a paragraph on the live channel
  (socket, env, threading).
- `README.md`: "Agent channel": live versus headless, the two variables,
  what a workspace's agent may reach.
- `PLUGINS.md`: `View::agent_reachable` in the `View` hooks; "Hidden
  surfaces" no longer says `get tree` lists them for a scoped agent.
- `TODO.md`:
  - "Agent channel → the running app (+ MCP server)": narrowed to what's
    left (MCP shim, standing grant, agent web profiles, peer-ancestry
    binding, a desktop check of macOS env readability).
  - "Web — revisit the person/agent pop-up rule if agents drive the GUI":
    `Requires` becomes agent web profiles; agents can't reach pages in the
    window yet.
  - "Agent screenshot command": `Requires` names the scoped channel;
    unchanged otherwise.

## Commits (branch `feat/agent-channel-live`)

1. `feat(core): agent reach on command metadata`: `AgentReach`, the default,
   the builders, and the classification of every command (nothing reads it
   yet).
2. `feat(terminal-ghostty): remove an env variable from a spawned command`.
3. `feat(app): scope agent requests to a workspace`: `View::agent_reachable`,
   acting workspace, `agent::serve` with `Op` and authorize, scoped `get
   tree`; headless uses `Scope::Full`. Enforcement tests.
4. `feat(app): workspace tokens in terminal env`: minting, resolve with
   revocation, `OpenCx::agent_env`. Token and env tests.
5. `feat(app): agent socket server`: directory, bind, accept and connection
   threads, `hello`, caps, parked file replies, guarded removal. Socket and
   safety tests.
6. `feat(app): serve the agent socket from the window`: bind in
   `State::new` before the first terminal, `UserEvent::Agent`, drain,
   settle before report toasts, the warning toast.
7. `feat(app): ghostrealm agent connects to the running window`: client and
   its test.
8. `docs: the agent channel in the running app`.

## Size

| Part | Lines |
| --- | --- |
| Core meta + 53 classifications | 90 |
| `env_remove` + terminal env | 40 |
| Scope, acting workspace, `serve`, scoped tree | 260 |
| Tokens | 80 |
| Socket server + parked replies | 260 |
| Window wiring | 60 |
| Client | 70 |
| Tests | 650 |
| Docs | 90 |
| Total | about 1600 |

## Later (not in this increment)

None of these is needed for the increment to be safe or usable: an agent
reaches its own workspace, and everything it can't reach is refused.

- The MCP stdio shim over this socket.
- The per-workspace standing-grant toggle and any approval prompt; until
  then `workspace.new` and every other workspace stay out of reach.
- Agent web profiles, agent-opened browsers, sharing a pane with an agent,
  login handover. Then `web.*` gets a class that checks the page's profile.
- A config "hide from agent" list.
- Screenshots.
- Cookie encryption at rest for the person's profile.
- Binding a connection to the client's process ancestry (closes the Linux
  `/proc/<pid>/environ` gap).

## Open questions (default first; none adds scope)

1. **The refused list.** Default: the table above, notably clipboard, file
   clipboard, Reveal / Open in Default App, toasts, the picker, and
   `workspace.close`. Moving one to `Workspace` is a one-line change each.
2. **A dead socket.** Default: `ghostrealm agent` errors and exits 1 when the
   variable names a socket it can't reach. Falling back to headless would
   let an agent drive a copy unknowingly.
3. **Exited terminals live.** Default: follow the person's
   `close_on_exit`, so an agent can't read a terminal that closed itself.
   Keeping them for agents would change what the person sees.
4. **Confirmations raised by an agent's command** (Close with unsaved
   changes, Delete of an untracked file). Default: shown to the person, who
   answers. Refusing those commands instead would remove closing and
   deleting from agents.
5. **The Linux environment gap.** Default: accept and document for this
   increment; the ancestry binding comes later.
6. **macOS environment readability** (not a decision): check at a desktop
   that `ps -wwE -p <shell pid>` from a process outside the workspace shows
   no `GHOSTREALM_AGENT_TOKEN`.
