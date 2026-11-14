# Plan: the per-workspace standing grant

`decisions-pending.md` item 3, next increment. An autonomous agent job (testing
the app, say) reaches beyond its own workspace once the person has switched
that on for its workspace. The person sets it once and leaves it on, with no
approval per step. What is the person's alone stays out of reach whatever the
grant. Refuse by default stays: a workspace starts ungranted, and a command
nobody classified stays refused.

The owner's answer asks for "human approval, given via a per-workspace toggle".
The toggle is that approval. Per-request approval prompts are not built: the
need is a job that runs without stopping, and a prompt is the stop.

## Settled by the code today

- One enforcement point, `agent::serve` (`crates/app/src/agent.rs`).
  `authorize` checks each request against `Scope::Workspace(vt)`, and
  `execute` runs commands through `AppState::acting_as(vt, …)`
  (`app_state/scoped.rs`).
- The socket server resolves the token to its workspace on every request
  (`Server::answer`, `agent/socket.rs`), not once per connection. A check
  added there takes effect on the next request of a live connection.
- A surface is reachable when its view opts in (`View::agent_reachable`:
  terminals and Files surfaces) and it is in the agent's workspace. Web views
  never opt in. Hidden surfaces (pop-ups) are in no workspace.
- `CommandMeta::agent` is `Refused(why)` by default, or `Workspace` or
  `FocusedPane`.
- Workspaces don't survive a restart. Tokens are random per workspace per run.
- `acting` (the workspace a command runs as) does two jobs: it is the
  workspace commands act on (`AppState::active`), and it names the workspace
  in an agent's question (`asked_from`) and in `file.delete`'s check.
- Two bugs that opening `workspace.new` to agents would expose:
  - `new_workspace_in` sets the root through `set_active_root_dir`, which under
    `acting_as(A)` sets A's root, not the new workspace's.
  - `Tree::add_empty_vtab` makes the new workspace the person's active one.
- Pin/Unpin is a window-only action in the sidebar's workspace menu
  (`MenuAction::TogglePin`, `window.rs`), not a registry command. The toggle
  follows that precedent.

## What a granted workspace's agent reaches

| | Ungranted | Granted |
| --- | --- | --- |
| Terminals and Files surfaces (`get surface`, `input` by `id`, a command's `id` arg) | its workspace's | every workspace's |
| `get tree` | its workspace | every workspace, each listing only reachable surfaces; no hidden surfaces |
| Commands classified `Workspace` / `FocusedPane` | run as its workspace | run as its workspace, or as any open workspace named by a new optional `"workspace": N` on `set` |
| `workspace.new` | refused | with `dir`: creates the workspace without making it the person's active one, and replies with its id. Without `dir`: refused (the picker is the person's) |
| `workspace.close` | refused | runs on the workspace it acts as while every surface in it is empty or reachable; refused while it holds a web page or any view that doesn't opt in |
| `file.delete` of a path open elsewhere | refused if a document under it is open outside its workspace | refused if one is open in a surface it can't reach (none, today: documents live in Files surfaces) |
| `input` with no `id` | its workspace's focused surface | the same |

A workspace's grant covers the agents whose token is that workspace's. A
workspace a granted agent creates starts ungranted, and so do its terminals'
agents.

## What stays refused with the grant

`Refused(why)` keeps meaning "the person's alone": no grant opens it. The grant
opens only a new class, `Granted(why)`.

- Web pages: every `web.*` command (`WEB_PAGES`), and every web surface (web
  views don't opt in to reach). Structural: no request type reaches a surface
  that doesn't opt in, and no command makes a page reachable. Agent web
  profiles are item 4's.
- The person's desktop: `file.copy_path`, `file.copy_relative_path`,
  `file.reveal`, `file.open_default` (`DESKTOP`).
- The person's file clipboard: `file.cut`, `file.copy`, `file.paste`
  (`FILE_CLIP`).
- Modals that are the person's: `workspace.pick_dir`, `editor.save_as`,
  `file.save_copy_as`, `editor.open` with no `path`, `workspace.new` with no
  `dir` (`PICKER`).
- What the person is reading: `toast.dismiss`.
- Answering on the person's behalf: no op answers a question. A question a
  granted agent's command raises goes to the person as today: marked late, and
  named for the agent's workspace (the home), not the workspace it acted in.
- The grant itself (below).
- Every command nobody classified.

## The toggle

**Where the person sets it.** A row in the sidebar's workspace menu (right-click
a workspace): "Agents reach all workspaces", with a check mark while on.
Choosing it flips the grant for that workspace. It is not a registry command,
so it is in no agent's catalogue, the palette, or the keybindings.

**Indicator.** A granted workspace's sidebar row shows an amber bar along its
edge, the full row height, in every state (active, hovered, any status). The
colour and placement need a desktop check.

**No agent path sets or clears it.** The grant set lives in the window
(`Window::agent_grants`, an `agent::Grants`), not in `AppState`:

- Socket requests: `Server::drain` takes `&Grants` and builds each request's
  scope from it by value. Registry commands take `&mut AppState` and can't
  name a window field.
- Terminal input: an agent's `input` writes bytes to a surface. Nothing a
  terminal program prints (escape sequences included) reaches the grant.
- Views: a view reaches the window only through its `Request`s, and none
  toggles a grant. The menu row is built only by the sidebar's workspace menu,
  and fires only on a mouse release over it, which no agent op produces.
- Config: the grant is never read from a file. Everything on disk the app
  reads, an agent's same-user shell can write.
- Keybindings: none, since it is not a command. An agent could write a binding
  into `config.toml`, so a bindable toggle could sit under a chord the person
  presses for something else.

**Persistence.** In memory, for the workspace's life: closing the workspace or
quitting drops it. Workspaces don't survive a restart, so there is nothing to
reattach a stored grant to. Keying a stored grant to a root directory would
grant future workspaces the person hasn't looked at, and would need storage an
agent can't write (not a file; the Keychain). That is a feature beyond this
need.

**Switching it off.** It takes effect at the agent's next request, on live
connections too: the connection stays open and its reach falls back to its own
workspace. A request already served isn't undone, and a command runs on the UI
thread, so no toggle lands mid-command. File jobs a granted command started
finish and reply. A question it raised stays for the person. Switching off
doesn't revoke:

- programs the agent started in other workspaces' terminals: they hold those
  workspaces' tokens and act with those workspaces' own reach;
- tokens the agent read while granted, say by typing `env` into another
  workspace's terminal and reading it back. Each reaches its own workspace
  (and, for a granted workspace, everything the grant opens) until that
  workspace closes. Item 25's option B closes this.

## Protocol

- `set` takes an optional `"workspace": N`: run the command as workspace N.
  Ungranted, any N but its own is refused with "workspace N is outside this
  agent's workspace: the person hasn't let its agents reach other workspaces";
  granted, N must be open ("unknown workspace N").
- A `Granted` command refused for an ungranted agent says why and that the
  grant would open it.
- `help` and `search` list what the scope may run now, so they change with the
  grant.
- `get tree` replies with `"granted": true|false` for a workspace's agent.

## Items 25 and 26

Neither is built here. What the grant changes:

- **Item 25 (borrowed tokens).** A granted workspace's token reaches every
  workspace's terminals and Files surfaces and opens `workspace.new` / `close`.
  A same-user process that borrows it (Linux: `/proc/<pid>/environ`; macOS
  unverified) gets all that. A granted agent can also read every other
  workspace's token from their terminals and keep it after the grant is off.
  Option B (bind each connection to the client's process ancestry) closes
  both: a token used outside its workspace's process tree is refused. Worth
  settling before leaving a grant on for long jobs on Linux.
- **Item 26 (focus).** Ungranted, the hazard arises only when the agent's
  workspace is the person's active one. Granted, an agent anywhere can act as
  the workspace the person is typing in, and can `input` into its terminals
  directly, which is worse than moving focus. Whichever answer item 26 gets,
  its check keys on "the workspace the command acts as is the person's
  active", not on the agent's home. `workspace.new` would switch the person's
  active workspace, a focus steal outside item 26's list; this plan keeps the
  person's active workspace instead. `workspace.close` can close the workspace
  the person is in, moving their view to another.

## Steps

1. `crates/core/src/command.rs`: add `AgentReach::Granted(&'static str)` (runs
   as `Workspace` once granted; refused otherwise, for the reason) and
   `AgentReach::GrantedWorkspace(&'static str)` (as `Granted`, and only while
   every surface in the workspace it acts as is empty or reachable: it closes
   the workspace), with builders. Doc `Refused` as never opened by a grant.
2. `crates/app/src/agent.rs`:
   - `pub struct Grants(HashSet<VtabId>)`: `has`, `toggle`.
   - `Scope::Workspace { home: VtabId, granted: bool }`.
   - Parse `workspace` on `set`.
   - `authorize`: the target check; the two new classes; reach checks through
     the scope's reach (below).
   - `execute`: run as the target, with the home as the asker; `help` /
     `search` filter by the scope's classes; `get tree` for the scope, with
     `granted`.
   - Module docs: the `workspace` field and the grant.
3. `crates/app/src/app_state/scoped.rs` and `app_state.rs`:
   - `pub(crate) struct Agent { home, granted }`; `acting: Option<Acting>` with
     `Acting { target, agent }`. `active()` reads the target, `asked_from`
     names the home, and `agent_scope()` returns the `Agent`.
   - `agent_reaches(agent, sid)`: the view opts in, and the surface is in the
     home, or in any workspace when granted.
   - `focused_pane_reachable(agent, target)`; new
     `workspace_reachable(agent, target)`.
   - `describe_tree` takes the `Agent`: workspaces by grant, surfaces by
     `agent_reaches`.
   - `file.delete`'s check: refuse if a document under the path is in a surface
     the agent can't reach. `FileJob::DeleteCheck` carries the home for the
     question's name.
   - `workspace.new`: class `Granted`. Under an agent: refuse without `dir`,
     as `editor.open` does, and put back the person's active workspace
     (`Tree::focus_vtab`, not `AppState::focus_vtab`, which applies the inbox's read rules).
     `new_workspace_in` sets the root on the new workspace by id.
   - `workspace.close`: class `GrantedWorkspace`.
4. `crates/app/src/agent/socket.rs`: `drain` and `answer` take `&Grants`; the
   scope is built per request.
5. `crates/app/src/window.rs`:
   - `agent_grants: Grants`, passed to `drain`.
   - The workspace menu's rows from a pure function of status, pinned and
     granted, with the new checked row; `MenuAction::ToggleAgentGrant`.
   - The amber bar on a granted row.
6. Docs:
   - `DECISION.md`: a new Agents / security entry: the grant, what it opens and
     keeps refused, the toggle, persistence, switching off and what it doesn't
     revoke, no per-request prompts.
   - `TODO.md`, "Agent channel → the running app": drop the standing grant and
     approval prompts from "Left".
   - `SPEC.md` phase 3: drop "the standing grant".
   - `decisions-pending.md` item 3 **Built**: add the grant. Items 25 and 26:
     one background line each on what the grant changes.

No file is deleted or overwritten outside the edits above, and no command,
script or path is removed.

## Tests

Agent-channel tests in `agent.rs` drive `serve` as the socket does; socket
tests drive a real socket. Each guard has a test that drives it to refuse.

| Guard | Test (Given / when / then) |
| --- | --- |
| Ungranted stays scoped | Workspaces A and B with terminals, A ungranted; A's agent sets `split.leftright` with `workspace: B`, sets `workspace.new` with `dir`, and `workspace.close`: each is refused, and B, the workspace list and the active workspace are unchanged. The existing `web_pages_and_other_workspaces_are_out_of_an_agents_reach` keeps covering surfaces. |
| Granted reach | A granted; its agent reads and types into B's terminal, lists B in the tree, splits B with `workspace: B`: B gains a pane, A doesn't. `help` lists `workspace.new` and `workspace.close`. |
| Granted keeps `Refused` | For every registry command classed `Refused`, a granted agent is refused with that command's own reason. |
| Web pages, granted | A person's web page in B: a granted agent's tree omits it, `get surface` / `input` on it say `unknown surface N`, `workspace.close` as B is refused and the page stays. |
| `workspace.new`, granted | Without `dir`: refused, no picker. With `dir`: replies with the new id; the person's active workspace and A's root are unchanged; the new workspace's root is `dir`. A terminal opened there: its token's agent is ungranted (refused with `workspace: A`). |
| `workspace.close`, granted | Workspace C with terminals only: closed. Its token is then refused. |
| Unknown target | Granted, `workspace: 999`: "unknown workspace 999". |
| Question names the home | A granted, file edited in B; A's agent deletes it with `workspace: B`: the question starts "From workspace “A”". |
| Switching off, live connection | Over a real socket: with the grant, B's tree and a split of B succeed; the grant is toggled off between requests on the same connection; the next request for B is refused, a request for A is served, and the connection stays open. |
| Pins | `OPEN_TO_AGENTS` grows a granted list (`workspace.new`, `workspace.close`) and the pin test covers both classes. |
| Menu row | The workspace menu's rows for a granted and an ungranted workspace: the toggle row is present, checked as granted. |

The guard against an agent setting the grant is the type: `drain` takes
`&Grants`, and commands take `&mut AppState`. No runtime test can express a
command setting it.

Run: `cargo test -p ghostrealm-core`, `cargo test -p ghostrealm agent`,
`cargo test -p ghostrealm socket`, then the app crate's full tests and
`cargo clippy`, per the build skill. No OS-specific code: the socket module is
Unix-wide already.

## Size

About 550 lines, insertions plus deletions: code ~230 (core ~20, `agent.rs`
~80, `scoped.rs` / `app_state.rs` ~60, `socket.rs` ~15, `window.rs` ~45), tests
~280, docs ~40.

## Desktop check

- The menu row: label, check mark, hover highlight, fires on release.
- The amber bar on a granted row, in active, hovered and each status, and with
  a two-line (named) row.
- Live: two workspaces with terminals; grant A; from `ghostrealm agent` in A,
  `{"op":"get","what":"tree"}` lists both; `{"op":"set","id":"workspace.new","args":{"dir":"/tmp"}}`
  adds a workspace without switching the person's view;
  `{"op":"set","id":"split.leftright","workspace":<B>}` splits B; switch the
  grant off in the menu and send the split again on the same session: refused.
- Closing a workspace from another: `workspace.close` with `workspace: <C>`.

## Risks

- A granted agent can rename any workspace, including to "settings", which
  `open_settings` finds by name (TODO, "Agent socket — findings recorded from
  the review"). The grant widens an existing finding; not fixed here.
- The 32-connection cap is window-wide (same TODO entry); unchanged.
- `acting` changes shape; every reader (`active`, `asked_from`, `file.delete`,
  `editor.open`) moves with it. The existing scoped tests cover them.

## Open questions

1. A palette entry or keybinding for the toggle? Recommend no: the menu meets
   the need, a palette entry needs a catalogue of the person's own commands
   apart from the registry, and a bindable toggle could be put under a chord by
   an agent editing `config.toml`. Another answer adds a feature.
2. Persist across restarts? Recommend no (above). Another answer adds a store
   outside an agent's write access.
3. Does an agent's `workspace.new` leave the person's active workspace as it
   was? Recommend yes; item 26's answer may revisit it. Switching to the new
   workspace, as the person's own `workspace.new` does, puts the person's keys
   in the agent's workspace.
4. Does the grant reach the terminals and Files surfaces of a workspace that
   also holds the person's web pages? Recommend yes: the pages stay
   unreachable and `workspace.close` there is refused.
5. A toast when the grant is switched on? Recommend no: the bar and the check
   mark show it. Another answer adds UI.
6. Open the file clipboard to granted agents? Recommend no: it is the person's.
   Another answer adds reach.
7. Name: "standing grant" in docs, "Agents reach all workspaces" in the menu.
   Add "Standing grant" to `GLOSSARY.md` once the owner agrees the name.
