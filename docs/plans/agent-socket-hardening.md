# Plan: agent socket hardening

An agent driving the running app must not, by accident or misbehaviour, lock
other workspaces' agents out, grow the window's memory without bound, or
leave socket files behind, so the channel stays dependable for long
autonomous jobs. Source: `docs/TODO.md` entries "Agent socket — findings
recorded from the review" and "Agent MCP server — the 2026-07-28 revision
and review findings". The "Standing grant" entry holds no finding in this
scope. Out of scope: decisions-pending items 25, 26 and 27, and the
workspace-identity findings.

## Confirmed against the code (develop 23c49cc)

1. `crates/app/src/agent/socket.rs`: `MAX_CONNECTIONS = 32` is checked in
   `Accept::admit` over every connection, with or without a `hello`. No read
   timeout is ever set; `read_line` blocks until a line or EOF.
2. `Op::Input` (`crates/app/src/agent.rs`) calls `AppState::write_input` →
   `TerminalView::write_input` → `ThreadedTerminal::write_bytes`, which
   pushes `Cmd::Bytes` onto an unbounded `mpsc` channel and returns. The
   worker's `write_all` blocks while the child isn't reading, so the queue
   grows by up to 1 MiB per request, each answered `ok` at once.
3. `Server::bind_with` names each socket `<pid>-<8 hex>.sock` and removes only
   its own on close. A killed or crashed window's file stays.
4. `an_unreadable_directory_keeps_the_socket` sets the directory to 0200.
   Without search permission `unlink` fails as well as `lstat`, so the test
   passes with the `identity` guard in `remove_if_bound` removed.
5. `mcp::call` sends any request line. Over `MAX_LINE` the socket replies
   "line too long" and closes, so this call or the next fails with "the
   window hung up", and the shim exits 1. The plain relay (`client::relay`)
   sends the line too and ends the same way.
6. A long `$TMPDIR` or `$XDG_RUNTIME_DIR` makes `UnixListener::bind` fail
   with std's "path must be shorter than SUN_LEN", toasted without the path
   or a remedy.

## Changes

### 1. Connection limits (`agent/socket.rs`, `agent/client.rs`)

- `MAX_PER_WORKSPACE = 8`: connections past `hello` per workspace. An agent
  session holds one (the MCP shim) and `ghostrealm agent` one while it runs;
  8 leaves room for several sessions and relays in one workspace.
- `MAX_CONNECTIONS = 64` (from 32): bounds threads; 8 workspaces can fill
  their caps before it is reached.
- `HELLO_TIMEOUT = 10 s`: the deadline for the whole first line, from accept.
  Clients send `hello` on connect; 10 s covers a loaded machine and matches
  `WRITE_TIMEOUT`.
- `IDLE_TIMEOUT = 5 min`: the deadline for each later line, from the last
  reply written. An agent at work calls every few seconds; a workspace whose
  sessions went quiet gets its slots back within minutes. Reconnecting is
  cheap, so nothing gains from a longer one.

Mechanics:

- `Shared::conns` becomes `HashMap<u64, Conn { stream, workspace: Option<u64> }>`.
  After the UI thread answers `hello` with `ok`, the connection thread reads
  its `workspace`, and under the `conns` lock counts entries with that
  workspace. At the cap it writes `{"ok":false,"error":"too many connections
  for this workspace"}` instead of the `hello` reply and closes; otherwise it
  records the workspace. `Drop for Connection` already removes the entry, so
  the count frees itself.
- Deadlines, not per-read timeouts (a client sending one byte every few
  seconds would never time out): `BufReader` wraps a `Timed` reader that sets
  the stream's read timeout to the time left before each `read`, and returns
  `TimedOut` once none is left. `read_line` maps `TimedOut`/`WouldBlock` to a
  new `Read::Idle`.
- First line timed out: close without a reply. A later line timed out: write
  `{"ok":false,"error":"closed after 5 minutes without a request","closed":"idle"}`
  and close. The server writes that line only from the timed-out branch, so it
  proves no request on that connection was read after the last reply.
- `client::Connection` keeps the socket path and token. In `request`, a reply
  carrying `"closed":"idle"` (read after the write, or after a write that
  failed with `EPIPE` because the server already closed) means the request
  was never read: reopen (`hello` again) and send it once more. Any other
  failure stays an error, as today. This keeps the MCP shim and the relay
  working across idle gaps.
- `Connection::open`'s refusal reads "the window refused the connection: …",
  since the per-workspace cap now refuses a valid token too.
- `bind_with` takes a `Limits { recheck, hello, idle }` in place of
  `recheck`, so tests can use short timeouts.

Not added: a separate cap on connections that haven't sent `hello`. A flood of
them fills `MAX_CONNECTIONS` and refuses new connections, which a pending cap
would do too; existing connections are unaffected either way, and each
pending slot frees within `HELLO_TIMEOUT`. Telling who floods needs the
peer's pid, which is decisions-pending item 25's option.

### 2. Bounded terminal input (`terminal-ghostty/src/threaded.rs`, views, `agent.rs`)

Bound the queue (recommended) rather than reply once written. Replying once
written needs a written-up-to counter from the worker, a new parked reply kind
in `Server`, and a wake from the worker: plumbing through four layers for the
same protection.

- `threaded::Shared` gets `queued: AtomicUsize`. `write_bytes` and `paste` add
  their length before sending; the worker subtracts it after
  `term.write_bytes` / `term.paste` returns. `ThreadedTerminal::input_queued()`
  reads it.
- `View::input_queued(&self) -> usize`, default 0 (`plugin/mod.rs`);
  `TerminalView` returns its terminal's. `AppState::input_queued(id)`.
- `MAX_INPUT_QUEUED = 1 MiB` (`agent.rs`), the size of one request line: in
  `Op::Input`, when `queued + text.len()` exceeds it, reply
  `{"ok":false,"error":"surface N isn't reading its input: K bytes still queued"}`
  and write nothing. A terminal's queue then holds at most 1 MiB from agents.
  The person's typing and pastes are counted, so an agent waits behind a
  large paste too, but are never refused.
- `docs/PLUGINS.md`: add `input_queued` to the view defaults sentence.

### 3. Sweep dead windows' sockets (`agent/socket.rs`)

`sweep(dir, uid, running)`, called in `bind_with` after `checked_dir` succeeds
and before the bind, on macOS only (`#[cfg(target_os = "macos")]` on the
call; see "Linux"). `running` is `kill(pid, 0)`: alive on success or `EPERM`;
dead only on `ESRCH`; any other error is alive.

A directory entry is removed only if every check passes, in this order; any
error, or a check that can't be made, keeps it:

1. `read_dir(dir)` succeeds; a failed entry is skipped.
2. Its name is exactly `<pid>-<8 lowercase hex>.sock`, `<pid>` a decimal `u32`
   (the name `bind_with` gives).
3. `symlink_metadata` (no symlink followed) says it is a socket.
4. It is owned by `uid`.
5. `running(pid)` is false.
6. `UnixStream::connect` to it fails with exactly `ECONNREFUSED` (no listener).
   A connection that succeeds is dropped at once.
7. `remove_file`; its error is ignored.

`dir` itself has passed `checked_dir`: a real directory, not a symlink, owned
by `uid`, mode `& 0o077 == 0`. Nothing else in it is touched and the sweep
doesn't recurse.

What each input trusts, and how it can be wrong:

| Input | Wrong when | Effect |
| --- | --- | --- |
| `read_dir` | error (permissions, sandbox) | nothing removed |
| name | another same-user program makes a lookalike name in the 0700 dir | still needs checks 3–6: only a dead, listener-less socket goes |
| `lstat` type | a symlink in the socket's place | not a socket: kept; its target is never read |
| `lstat` owner | root or another user placed a file there | uid differs: kept |
| `kill(pid, 0)` | pid reused by another process | reads alive: kept (a stale file lingers until next start) |
| `kill(pid, 0)` | the window is in another pid namespace sharing the dir (Linux sandboxes, containers) | reads dead; check 6 catches a listening socket |
| `connect` | sandbox denies it (`EPERM`, `EACCES`) | not `ECONNREFUSED`: kept |
| `connect` | a window between `bind` and `listen` gives `ECONNREFUSED` | its pid is alive: kept by check 5 |

Races:

- A window starting at the same time binds `<its pid>-<random>.sock`; its pid
  is alive, so check 5 keeps it.
- Two windows sweeping at once both remove the same dead file; the second
  `remove_file` fails with `ENOENT`, ignored.
- Between checks 3–6 and the unlink the file could be replaced only by a
  process binding that exact name: a new window that reused the dead pid and
  drew the same 32-bit random suffix, or a same-user process doing it on
  purpose (which can delete the file anyway).
- Both check 5 and check 6 wrong at once needs a window in another pid
  namespace, sharing the directory, caught between `bind` and `listen`. No
  pid namespaces on macOS, where the sweep runs.

### 4. Make the unreadable-socket test reach its guard

`remove_if_bound(path, bound)` becomes `remove_if_bound(path, bound, now:
io::Result<(u64, u64)>)`; its callers pass `identity(path)`. The test
`an_unreadable_directory_keeps_the_socket` is replaced by
`an_error_reading_the_socket_keeps_it`: a bound socket, a normal directory,
`remove_if_bound(&path, Some(id), Err(..))`, and the file is kept. Removing
the `now` check removes the file, so the test fails.

### 5. Refuse an over-long request in the client (`agent/client.rs`)

`MAX_LINE` becomes `pub(crate)`. `Connection::request` checks
`line.len() + 1 > MAX_LINE` first and returns the reply line
`{"ok":false,"error":"request too long: N bytes; the window takes at most 1048576"}`
without sending. The MCP shim turns that reply into a tool error
(`tool_result`) and keeps serving; the relay writes it and reads on. One
check covers both; `mcp.rs` is unchanged.

### 6. Socket path too long (`agent/socket.rs`)

Before binding, compare the path's byte length with `sun_path`'s size (from
`libc::sockaddr_un`, 104 on macOS, 108 on Linux, one byte for the NUL) and
fail with "socket path <path> is N bytes, over the M this system allows; set
XDG_RUNTIME_DIR or TMPDIR to a shorter directory". The window's existing
toast shows it. About 10 lines.

## Tests

Each is new unless marked, and each fails with its guard removed.

| Test (file) | Drives | Fails without |
| --- | --- | --- |
| `a_workspace_past_its_connection_cap_is_refused_and_others_are_not` (socket.rs) | 8 connections with A's token; a 9th with A's is refused and hung up on; one with B's is served; after one of A's closes, a new A connection is served | per-workspace cap (9th served); the count freed on drop (last one refused) |
| `a_connection_past_the_limit_is_refused` (socket.rs, rewritten) | `MAX_CONNECTIONS` connections that never say `hello`, admitted; one more is refused | window-wide cap |
| `a_connection_without_a_hello_is_closed_at_its_deadline` (socket.rs) | hello timeout 200 ms; one silent client, one sending a byte of `hello` every 50 ms; both hung up within 2 s | first-line timeout (silent client); deadline rather than per-read timeout (trickling client) |
| `an_idle_connection_is_closed_and_the_client_reopens_it` (socket.rs) | idle timeout 300 ms; a raw client past `hello` gets the idle line, then EOF; a `client::Connection` sends a request, sleeps past the timeout, sends another and gets its reply | idle timeout (no idle line); client reopen (second request errors) |
| `a_request_the_window_closed_unread_is_sent_again_once` (client.rs) | fake listener: answers `hello`, then on the next request writes the idle line and closes without reading; on the second connection answers `hello` and the request. Asserts one reply, from the second connection | resend on an idle line read as the reply |
| `input_to_a_terminal_not_reading_is_refused_past_the_bound` (agent.rs) | terminal running `stty raw -echo; echo ready; exec sleep 30`, after `ready`: input of 600 KiB is `ok`; a second of 600 KiB is refused, naming the queued bytes | `MAX_INPUT_QUEUED` check |
| `queued_input_drains_once_written` (threaded.rs) | `cat` child; write 64 KiB; `input_queued()` reaches 0 | the worker's subtraction (counter stuck, refusing forever) |
| `stale_sockets_of_dead_windows_are_swept` (socket.rs) | in one dir, `running` injected: a dead, listener-less `<pid>-<hex>.sock` is removed; kept: near-miss names (`<pid>.sock`, `<pid>-abc.sock`, `<pid>-deadbeef.sock.old`, `x<pid>-deadbeef.sock`), a regular file with a matching name, a matching-name symlink to a stale socket outside the dir (link and target kept), a stale socket whose pid `running` says is alive, a socket with a live listener (still accepts afterwards), a stale socket with mode 000 (`connect` gives `EACCES`) | name pattern; `lstat` type; symlink not followed; pid check; `ECONNREFUSED` only (listener); `ECONNREFUSED` only (other errors) |
| `the_sweep_keeps_another_users_sockets` (socket.rs) | `sweep(dir, uid() + 1, \|_\| false)` keeps a stale, matching socket | owner check |
| `a_live_or_unsignalable_pid_is_running` (socket.rs) | `running(1)` (EPERM as non-root) and own pid are true; a spawned, reaped child's pid is false | `EPERM` read as alive |
| `a_directory_that_isnt_the_users_alone_is_refused` (socket.rs, extended) | a stale matching socket in the 0755 dir and in the symlink's target is kept after the refused bind | sweep only inside a checked directory |
| `an_error_reading_the_socket_keeps_it` (socket.rs, replaces `an_unreadable_directory_keeps_the_socket`) | see change 4 | `identity` guard in `remove_if_bound` |
| `an_over_long_request_is_refused_without_closing` (socket.rs) | MCP: a `tools/call` `input` over `MAX_LINE`, then `get tree`: a tool error naming the limit, then the tree, and `serve` returns `Ok`; relay: an over-long line then `pump`: the error line, then `{"ok":true}`, `Ok` | client-side length check |
| `a_socket_path_past_sun_path_is_refused_clearly` (socket.rs) | a directory whose path pushes the socket past `sun_path`: the bind error names the path and `XDG_RUNTIME_DIR` | the length check (std's message lacks both) |

`an_over_long_line_is_refused_and_closed` stays: the socket still refuses
over-long lines from other clients.

Run: `cargo test -p ghostrealm-app agent` and
`cargo test -p ghostrealm-terminal-ghostty threaded` with
`GHOSTREALM_NO_WEB=1`, then `cargo test --workspace` and `cargo clippy
--workspace --all-targets`. Nothing needs a desktop.

## Linux

The sweep deletes files and this machine can't run Linux, so its call is
`#[cfg(target_os = "macos")]` and `docs/TODO.md` gets an entry: run the sweep
tests on Linux, then enable it there (where check 6 matters, for pid
namespaces). The function and its tests compile on both. The `sun_path` check
and the rest are portable POSIX: compile-checked on a scratchpad copy with
`target_os` swapped, per the build skill.

## Docs

- `docs/DECISION.md` (Agents / security): one Detail line on the
  per-workspace cap, the `hello` and idle deadlines, the idle line and the
  client's reopen, and the input bound.
- `README.md` (agent section): an idle connection is closed after 5 minutes;
  `ghostrealm agent` and the MCP server reopen it.
- `agent/socket.rs` module doc: the limits and the sweep.
- `docs/PLUGINS.md`: `input_queued`.
- `docs/TODO.md`: delete the six fixed Details (cap/timeouts, sweep, the
  unreadable test, `sun_path`, `input`, the MCP `MAX_LINE` one); add the Linux
  sweep entry.

## Removed or replaced

No command, script or path is removed.

- `Server::bind_with(dir, uid, waker, recheck)` → `bind_with(dir, uid, waker,
  Limits)`; its one test caller (`a_socket_deleted_while_serving_is_bound_again`)
  passes `Limits { recheck: 50 ms, ..Limits::default() }`.
- `remove_if_bound(path, bound)` → `remove_if_bound(path, bound, now)`; the
  duty of reading the file moves to its two callers.
- Test `an_unreadable_directory_keeps_the_socket` → `an_error_reading_the_socket_keeps_it`.

## Risks

- An idle line racing a request: the resend relies on the server writing that
  line only when it read nothing; the client test and the server's single
  timed-out branch pin it.
- A reopen refused at the workspace cap is an error, so the MCP shim exits as
  it does on any hang-up today. Needs more than 8 connections in one
  workspace.
- `input_queued` counts the person's pastes: a large paste into a terminal
  that isn't reading makes an agent's `input` wait until it drains.
- Timing tests (deadlines of 200–300 ms) on a loaded machine: assert upper
  bounds with seconds of slack.

## Size

About 570 lines of code and tests (insertions plus deletions): socket.rs
≈ 400 (≈ 150 code, ≈ 250 tests), client.rs ≈ 85, terminal input bound
≈ 85. Docs ≈ 40 besides this plan.

## Open questions (recommended answer first)

1. Input: bound the queue (recommended, above), or reply once written (adds a
   parked reply kind and a worker-to-UI written counter).
2. Idle timeout: keep it, with the client's reopen (recommended; the brief
   asks for it). Dropping it saves ≈ 85 lines (`client.rs` reopen and the idle
   tests) and leaves quiet sessions holding their workspace's slots.
3. A refused reopen ends the MCP shim (recommended: same as any hang-up).
   Keeping it alive to retry on the next call adds a feature.
4. `sun_path` check: include (recommended, ≈ 20 lines with its test).
