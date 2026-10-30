//! A terminal that runs its VT engine on a dedicated worker thread.
//!
//! [`GhosttyTerminal`] is `!Send` (it owns the VT engine and its render
//! iterators), which forbids *moving/sharing* it across threads — but a worker
//! thread that *creates and solely owns* one never crosses a thread boundary, so
//! `!Send` is a non-issue. This wrapper does exactly that: the worker owns the
//! VT + PTY, reads and parses child output, builds [`Grid`] snapshots (plain
//! `Send` data), and publishes the latest to a mailbox the UI reads. The UI sends
//! input/resize/scroll as messages. The result: no VT parsing or snapshotting on
//! the UI thread, so rendering never blocks on terminal work.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use ghostrealm_terminal::{Grid, KeyPress, Lifecycle, MouseReport, Pumped, Scroll, TerminalBackend};

use crate::{lifecycle_of, CommandBuilder, GhosttyTerminal, PtyWaker};

/// How long a dropped terminal's child gets to exit (on the EOF and hangup
/// its closed PTY sends) before its and the foreground command's process
/// groups are sent SIGHUP. A worker blocked writing to a child that isn't
/// reading never closes the PTY; the child's exit fails the write.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// How long after SIGHUP before SIGKILL goes to the same groups (either may
/// ignore SIGHUP).
const KILL_GRACE: Duration = Duration::from_secs(1);

/// How long after the drop the reaper gives up on a child it can't reap
/// (one stuck in the kernel), leaving it unreaped.
const REAP_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the reaper checks on a dropped terminal's child.
const REAP_POLL: Duration = Duration::from_millis(10);

/// Wakes the UI event loop after the worker publishes a change.
pub type UiWaker = Arc<dyn Fn() + Send + Sync>;

/// A message to the worker thread.
enum Cmd {
    Key(KeyPress),
    Bytes(Vec<u8>),
    Paste(String),
    Mouse(MouseReport),
    Resize {
        cols: u16,
        rows: u16,
        cell_w: u32,
        cell_h: u32,
    },
    Scroll(Scroll),
    /// The PTY reader forwarded output; drain + parse it.
    PtyReady,
    /// The child exited (not yet reaped): refresh the lifecycle.
    ChildExited,
    Shutdown,
}

/// Cheap terminal state the UI reads without touching the VT.
#[derive(Clone)]
struct Meta {
    title: Option<String>,
    busy: bool,
    lifecycle: Lifecycle,
    /// The program enabled mouse tracking.
    mouse_tracking: bool,
    /// The viewport shows the live screen, not scrollback.
    viewport_active: bool,
    /// The PTY's foreground process group, for a forced shutdown.
    foreground: Option<i32>,
}

/// Shared between the worker (writer) and the handle (reader).
struct Shared {
    /// The most recent grid the worker built; the UI takes it. `None` once taken,
    /// until the worker publishes again.
    grid: Mutex<Option<Grid>>,
    meta: Mutex<Meta>,
    /// Bumped on every grid publish — the UI uses it to detect "new output" and
    /// whether a re-snapshot is due.
    generation: AtomicU64,
    /// What may be done with the child's pid. Whoever reaps the child or
    /// signals its process group holds this lock and checks it first, so no
    /// reap or signal reaches the pid once reaped (and maybe reused). That
    /// holds only while lock holders are the child's only reapers: nothing
    /// else in the process may call `wait` or `waitpid(-1)`, or
    /// set SIGCHLD to `SIG_IGN` (which reaps children on exit).
    child: Mutex<ChildState>,
}

/// Where the child's pid stands; see [`Shared::child`].
#[derive(Clone, Copy, Debug, PartialEq)]
enum ChildState {
    /// The exit waiter may yet wait on the pid, so nobody reaps it.
    Watched,
    /// The exit waiter is done with the pid; whoever finds it exited reaps it.
    Reapable,
    /// Reaped, and how it ended; the pid may now be another process's.
    Reaped(Lifecycle),
}

impl Shared {
    fn new() -> Self {
        Shared {
            grid: Mutex::new(None),
            meta: Mutex::new(Meta {
                title: None,
                busy: false,
                lifecycle: Lifecycle::Running,
                mouse_tracking: false,
                viewport_active: true,
                foreground: None,
            }),
            generation: AtomicU64::new(0),
            child: Mutex::new(ChildState::Watched),
        }
    }
}

/// UI-thread handle to a worker-backed terminal. `Send` (only channels + `Arc`s).
pub struct ThreadedTerminal {
    cmd_tx: Sender<Cmd>,
    shared: Arc<Shared>,
    /// Grid generation last observed by `pump` (drives the unread/output signal).
    last_pumped_gen: u64,
    /// Grid generation last consumed by a snapshot.
    last_snapshot_gen: u64,
    /// Last grid seen, for `snapshot()` (peek) callers like the agent.
    last_grid: Grid,
    /// Kept outside the worker, so the child is reaped even if it panics.
    child_pid: Option<u32>,
}

impl ThreadedTerminal {
    /// Spawn a terminal whose VT runs on a worker thread. `ui_waker`, if given, is
    /// called after the worker publishes a change so the event loop can redraw.
    pub fn spawn(
        cols: u16,
        rows: u16,
        cell_w: u32,
        cell_h: u32,
        command: Option<CommandBuilder>,
        ui_waker: Option<UiWaker>,
    ) -> Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let shared = Arc::new(Shared::new());
        let (start_tx, start_rx) = mpsc::channel::<Result<Option<u32>>>();
        let worker_shared = Arc::clone(&shared);
        // The PTY reader wakes the worker (not the UI) by posting `PtyReady`.
        let waker_tx = cmd_tx.clone();
        let exit_tx = cmd_tx.clone();

        std::thread::Builder::new()
            .name("vt-worker".into())
            .spawn(move || {
                let pty_waker: PtyWaker = {
                    let tx = waker_tx;
                    Box::new(move || {
                        let _ = tx.send(Cmd::PtyReady);
                    })
                };
                let started =
                    GhosttyTerminal::spawn_with_waker(cols, rows, cell_w, cell_h, command, Some(pty_waker));
                let mut term = match started {
                    Ok(t) => t,
                    Err(e) => {
                        let _ = start_tx.send(Err(e));
                        return;
                    }
                };
                match term.child_pid() {
                    Some(pid) => {
                        if let Err(e) = spawn_exit_waiter(pid, &worker_shared, exit_tx) {
                            let _ = start_tx.send(Err(e));
                            // Nothing waits on it, so its pid is still ours.
                            let mut child = term.into_child();
                            let _ = child.kill();
                            let _ = child.wait();
                            return;
                        }
                    }
                    None => *worker_shared.child.lock().unwrap() = ChildState::Reapable,
                }
                let _ = start_tx.send(Ok(term.child_pid()));
                worker_loop(&mut term, cmd_rx, &worker_shared, ui_waker.as_ref());
            })
            .map_err(|e| anyhow!("spawn vt-worker: {e}"))?;

        let child_pid = match start_rx.recv() {
            Ok(Ok(started)) => started,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(anyhow!("vt-worker died during startup")),
        };
        Ok(ThreadedTerminal {
            cmd_tx,
            shared,
            last_pumped_gen: 0,
            last_snapshot_gen: 0,
            last_grid: Grid::empty(),
            child_pid,
        })
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.cmd_tx.send(cmd);
    }
}

/// Dropping returns at once (it runs on the UI thread). The worker drops the
/// PTY on `Shutdown`, which sends the child EOF and hangs up its terminal; a
/// reaper thread reaps the child once it exits, sending SIGHUP if it hasn't
/// after [`SHUTDOWN_GRACE`], then SIGKILL after [`KILL_GRACE`], to its process
/// group and the foreground command's (checked to still be in the child's
/// session). It works from the pid alone, so a panicked worker's child is
/// reaped too, and signals only while the pid is unreaped.
impl Drop for ThreadedTerminal {
    fn drop(&mut self) {
        self.send(Cmd::Shutdown);
        let Some(pid) = self.child_pid.and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 1) else {
            return;
        };
        let shared = Arc::clone(&self.shared);
        let _ = std::thread::Builder::new().name("vt-reaper".into()).spawn(move || reap_dropped(&shared, pid));
    }
}

/// Reap the dropped terminal's child `pid`, escalating as [`ThreadedTerminal`]'s
/// drop describes; give up after [`REAP_TIMEOUT`], leaving it.
fn reap_dropped(shared: &Shared, pid: i32) {
    let start = Instant::now();
    let mut sent = None;
    loop {
        let elapsed = start.elapsed();
        let due = if elapsed >= SHUTDOWN_GRACE + KILL_GRACE {
            Some(libc::SIGKILL)
        } else if elapsed >= SHUTDOWN_GRACE {
            Some(libc::SIGHUP)
        } else {
            None
        };
        let signal = due.filter(|s| sent != Some(*s));
        let foreground = shared.meta.lock().map_or(None, |m| m.foreground);
        if reap_step(shared, pid, signal, foreground) {
            return;
        }
        sent = due;
        if elapsed >= REAP_TIMEOUT {
            return;
        }
        std::thread::sleep(REAP_POLL);
    }
}

/// Reap the child `pid` if it has exited and is [`ChildState::Reapable`];
/// otherwise send `signal`, if any, to its and the `foreground` process
/// groups. Returns whether there is nothing left to do: the child is reaped,
/// or its pid can't be trusted to be the child's (a poisoned lock, or a
/// `waitid` that doesn't find it), in which case it is left alone.
fn reap_step(shared: &Shared, pid: i32, signal: Option<libc::c_int>, foreground: Option<i32>) -> bool {
    // A panic under the lock may have hidden a reap.
    let Ok(mut state) = shared.child.lock() else {
        return true;
    };
    if let ChildState::Reaped(_) = *state {
        return true;
    }
    let Some(exited) = exited_unreaped(pid) else {
        return true;
    };
    if exited && *state == ChildState::Reapable {
        if let Some(done) = reap(pid) {
            *state = ChildState::Reaped(done);
        }
        return true;
    }
    // Unreaped, and only a holder of this lock reaps it: `pid` is the child's.
    if let Some(signal) = signal {
        signal_groups(pid, foreground, signal);
    }
    false
}

/// Whether the child `pid` has exited, left unreaped; `None` if `waitid`
/// doesn't find it among this process's unreaped children.
fn exited_unreaped(pid: i32) -> Option<bool> {
    loop {
        // SAFETY: waitid writes only the zeroed siginfo it is given.
        let (r, info) = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let flags = libc::WEXITED | libc::WNOWAIT | libc::WNOHANG;
            (libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags), info)
        };
        if r == 0 {
            // SAFETY: waitid set si_pid, to zero when nothing has exited.
            return Some(unsafe { info.si_pid() } != 0);
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return None;
        }
    }
}

/// Reap the exited child `pid`; `None` if `waitpid` doesn't.
fn reap(pid: i32) -> Option<Lifecycle> {
    loop {
        let mut status = 0;
        // SAFETY: waitpid writes only the status it is given.
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid {
            return Some(lifecycle_of(ExitStatus::from_raw(status)));
        }
        if r != -1 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return None;
        }
    }
}

/// Send `signal` to the process group of the child `pid` (it leads its own)
/// and to the `foreground` group. Call it only while `pid` is unreaped, so the
/// group is still the child's.
fn signal_groups(pid: i32, foreground: Option<i32>, signal: libc::c_int) {
    // SAFETY: killpg only sends a signal.
    unsafe {
        libc::killpg(pid, signal);
    }
    signal_foreground(pid, foreground, signal);
}

/// Send `signal` to the `foreground` group if it is still in the session of
/// the unreaped child `pid`: the group was read earlier and may since have
/// gone, its id reused.
fn signal_foreground(pid: i32, foreground: Option<i32>, signal: libc::c_int) {
    // SAFETY: getsid only reads a process's session id.
    let in_session = |group: i32| unsafe { libc::getsid(group) } == pid;
    if let Some(group) = foreground.filter(|g| *g > 1 && *g != pid && in_session(*g)) {
        // SAFETY: killpg only sends a signal.
        unsafe {
            libc::killpg(group, signal);
        }
    }
}

impl TerminalBackend for ThreadedTerminal {
    fn resize(&mut self, cols: u16, rows: u16, cell_w_px: u32, cell_h_px: u32) {
        self.send(Cmd::Resize {
            cols,
            rows,
            cell_w: cell_w_px,
            cell_h: cell_h_px,
        });
    }

    fn send_key(&mut self, press: &KeyPress) {
        self.send(Cmd::Key(press.clone()));
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        self.send(Cmd::Bytes(bytes.to_vec()));
    }

    fn paste(&mut self, text: &str) {
        self.send(Cmd::Paste(text.to_string()));
    }

    fn mouse_tracking(&self) -> bool {
        self.shared.meta.lock().unwrap().mouse_tracking
    }

    fn viewport_active(&self) -> bool {
        self.shared.meta.lock().unwrap().viewport_active
    }

    fn send_mouse(&mut self, report: &MouseReport) {
        self.send(Cmd::Mouse(*report));
    }

    fn scroll(&mut self, scroll: Scroll) {
        self.send(Cmd::Scroll(scroll));
    }

    /// The worker pumps on its own thread; here `pump` just reports whether new
    /// output has been published since the last call (drives the unread signal).
    fn pump(&mut self) -> bool {
        self.pump_budgeted(usize::MAX).changed
    }

    fn pump_budgeted(&mut self, _max_bytes: usize) -> Pumped {
        let gen = self.shared.generation.load(Ordering::Acquire);
        let changed = gen != self.last_pumped_gen;
        self.last_pumped_gen = gen;
        Pumped {
            changed,
            more: false,
        }
    }

    fn needs_snapshot(&self) -> bool {
        self.shared.generation.load(Ordering::Acquire) != self.last_snapshot_gen
    }

    fn snapshot_into(&mut self, out: &mut Grid) {
        let gen = self.shared.generation.load(Ordering::Acquire);
        if let Some(grid) = self.shared.grid.lock().unwrap().take() {
            *out = grid;
            self.last_grid = out.clone();
        }
        self.last_snapshot_gen = gen;
    }

    fn snapshot(&mut self) -> Grid {
        // Peek (don't consume) so repeated callers (the agent) keep seeing
        // output, and the paint path still takes the grid it hasn't drawn.
        if let Some(grid) = self.shared.grid.lock().unwrap().clone() {
            self.last_grid = grid;
        }
        self.last_grid.clone()
    }

    fn title(&self) -> Option<String> {
        self.shared.meta.lock().unwrap().title.clone()
    }

    fn is_busy(&self) -> bool {
        self.shared.meta.lock().unwrap().busy
    }

    fn lifecycle(&mut self) -> Lifecycle {
        self.shared.meta.lock().unwrap().lifecycle
    }
}

/// Start the thread that marks the child `pid` [`ChildState::Reapable`] and
/// tells the worker once it has exited.
fn spawn_exit_waiter(pid: u32, shared: &Arc<Shared>, tx: Sender<Cmd>) -> Result<()> {
    let shared = Arc::clone(shared);
    std::thread::Builder::new()
        .name("vt-exit-waiter".into())
        .spawn(move || {
            wait_for_exit(pid);
            if let Ok(mut state) = shared.child.lock() {
                if *state == ChildState::Watched {
                    *state = ChildState::Reapable;
                }
            }
            let _ = tx.send(Cmd::ChildExited);
        })
        .map(drop)
        .map_err(|e| anyhow!("spawn vt-exit-waiter: {e}"))
}

/// Block until the child `pid` has exited, leaving it unreaped (the worker,
/// or a dropped terminal's reaper, reaps it). Returns early if `waitid` fails
/// with anything but `EINTR`.
fn wait_for_exit(pid: u32) {
    loop {
        // SAFETY: waitid writes only the zeroed siginfo it is given.
        let r = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT)
        };
        if r == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return;
        }
    }
}

/// The child's lifecycle, reaping it if it has exited and is
/// [`ChildState::Reapable`]. While it is `Watched`, the waiter sends
/// `ChildExited` once that changes, and this runs again.
fn reap_if_exited(term: &mut GhosttyTerminal, shared: &Shared) -> Lifecycle {
    // A panic under the lock may have hidden a reap.
    let Ok(mut state) = shared.child.lock() else {
        return Lifecycle::Running;
    };
    match *state {
        ChildState::Watched => Lifecycle::Running,
        ChildState::Reaped(done) => done,
        ChildState::Reapable => {
            let now = term.lifecycle();
            if now.has_exited() {
                *state = ChildState::Reaped(now);
            }
            now
        }
    }
}

/// The worker's run loop: own the VT, apply commands, publish grids + meta.
fn worker_loop(
    term: &mut GhosttyTerminal,
    cmd_rx: Receiver<Cmd>,
    shared: &Arc<Shared>,
    ui_waker: Option<&UiWaker>,
) {
    let mut scratch = Grid::empty();
    // Block until there's something to do, then drain the whole batch so a flood of
    // PtyReady/commands collapses into one pump + one publish. Exits when the last
    // handle drops (channel closed) or on `Shutdown`.
    while let Ok(first) = cmd_rx.recv() {
        let mut pump = false;
        let mut shutdown = false;
        let mut apply = |m: Cmd, term: &mut GhosttyTerminal| match m {
            Cmd::Key(k) => term.send_key(&k),
            Cmd::Bytes(b) => term.write_bytes(&b),
            Cmd::Paste(t) => term.paste(&t),
            Cmd::Mouse(r) => term.send_mouse(&r),
            Cmd::Resize {
                cols,
                rows,
                cell_w,
                cell_h,
            } => term.resize(cols, rows, cell_w, cell_h),
            Cmd::Scroll(s) => term.scroll(s),
            Cmd::PtyReady => pump = true,
            // The meta refresh below reaps it.
            Cmd::ChildExited => {}
            Cmd::Shutdown => shutdown = true,
        };
        apply(first, term);
        while let Ok(m) = cmd_rx.try_recv() {
            apply(m, term);
        }
        if pump {
            term.pump();
        }

        // Publish a fresh grid if anything changed, recycling the previous
        // (possibly unconsumed) mailbox grid as the next scratch buffer.
        let mut published = false;
        if term.needs_snapshot() {
            term.snapshot_into(&mut scratch);
            let fresh = std::mem::replace(&mut scratch, Grid::empty());
            let prev = shared.grid.lock().unwrap().replace(fresh);
            scratch = prev.unwrap_or_else(Grid::empty);
            shared.generation.fetch_add(1, Ordering::Release);
            published = true;
        }

        // Refresh cheap meta (title/busy/lifecycle); wake the UI on any change.
        let new_meta = Meta {
            title: term.title(),
            busy: term.is_busy(),
            lifecycle: reap_if_exited(term, shared),
            mouse_tracking: term.mouse_tracking(),
            viewport_active: term.viewport_active(),
            foreground: term.foreground_group(),
        };
        let meta_changed = {
            let mut m = shared.meta.lock().unwrap();
            let changed =
                m.busy != new_meta.busy || m.lifecycle != new_meta.lifecycle || m.title != new_meta.title;
            *m = new_meta;
            changed
        };
        if published || meta_changed {
            if let Some(w) = ui_waker {
                w();
            }
        }
        if shutdown {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn sh(cmd: &str) -> CommandBuilder {
        let mut c = CommandBuilder::new("/bin/sh");
        c.arg("-c");
        c.arg(cmd);
        c
    }

    fn row_text(g: &Grid, row: u16) -> String {
        let mut s = String::new();
        for col in 0..g.size.cols {
            match g.cell(col, row) {
                Some(c) if !c.text.is_empty() => s.push_str(&c.text),
                _ => s.push(' '),
            }
        }
        s.trim_end().to_string()
    }

    #[test]
    fn worker_captures_output_without_ui_pumping() {
        // No pump() calls here — the worker parses on its own thread; the handle
        // just reads snapshots.
        let mut term =
            ThreadedTerminal::spawn(40, 10, 8, 16, Some(sh("printf READY-THREAD; sleep 2")), None)
                .expect("spawn threaded terminal");

        let deadline = Instant::now() + Duration::from_secs(4);
        let mut seen = false;
        while Instant::now() < deadline {
            let g = term.snapshot();
            if (0..10).any(|r| row_text(&g, r).contains("READY-THREAD")) {
                seen = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        assert!(seen, "the worker should parse child output without UI pumping");
    }

    #[test]
    fn input_reaches_the_child_via_the_worker() {
        let mut term =
            ThreadedTerminal::spawn(40, 10, 8, 16, Some(sh("cat")), None).expect("spawn");
        term.write_bytes(b"hello-thread\r");

        let deadline = Instant::now() + Duration::from_secs(4);
        let mut seen = false;
        while Instant::now() < deadline {
            let g = term.snapshot();
            if (0..10).any(|r| row_text(&g, r).contains("hello-thread")) {
                seen = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        assert!(seen, "input written to the handle should reach the child");
    }

    #[test]
    fn dropping_a_terminal_stuck_writing_does_not_block_and_kills_the_child() {
        let dir = std::env::temp_dir().join(format!("ghostrealm-reap-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let pid_file = dir.join("pid");
        let _ = std::fs::remove_file(&pid_file);
        // The child never reads its input, so a large write blocks the worker,
        // and it ignores SIGHUP.
        let cmd = sh(&format!(
            "trap '' HUP; stty raw -echo; echo $$ > '{}'; exec sleep 30",
            pid_file.display()
        ));
        let mut term = ThreadedTerminal::spawn(40, 10, 8, 16, Some(cmd), None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file).ok().and_then(|s| s.trim().parse::<i32>().ok()) {
                break pid;
            }
            assert!(Instant::now() < deadline, "the child never started");
            std::thread::sleep(Duration::from_millis(10));
        };
        term.write_bytes(&vec![b'x'; 1 << 20]);
        std::thread::sleep(Duration::from_millis(100));

        let start = Instant::now();
        drop(term);
        assert!(start.elapsed() < Duration::from_millis(100), "drop blocked: {:?}", start.elapsed());

        // Gone, or a zombie the reaper hasn't reaped yet.
        let deadline = Instant::now() + SHUTDOWN_GRACE + KILL_GRACE + Duration::from_secs(3);
        while running(pid) {
            assert!(Instant::now() < deadline, "the child outlived the grace period");
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_exit_without_output_is_noticed() {
        // The background job keeps the PTY open, so the exit brings no EOF, and
        // no command follows to wake the worker.
        let cmd = sh("sleep 3 & sleep 0.3");
        let mut term = ThreadedTerminal::spawn(40, 10, 8, 16, Some(cmd), None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        while term.lifecycle() == Lifecycle::Running {
            assert!(Instant::now() < deadline, "the exit was never noticed");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(term.lifecycle(), Lifecycle::Exited(Some(0)));
    }

    #[test]
    fn the_exit_waiter_leaves_the_child_to_reap() {
        let mut child = std::process::Command::new("sh").args(["-c", "sleep 0.2; exit 3"]).spawn().unwrap();
        let start = Instant::now();
        wait_for_exit(child.id());
        assert!(start.elapsed() >= Duration::from_millis(150), "it waited for the exit");
        let status = child.try_wait().unwrap().expect("exited, and still ours to reap");
        assert_eq!(status.code(), Some(3));
    }

    #[test]
    fn the_exit_waiter_returns_on_a_signal_death_leaving_the_child_to_reap() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = std::process::Command::new("sh").args(["-c", "sleep 0.2; kill -9 $$"]).spawn().unwrap();
        wait_for_exit(child.id());
        let status = child.try_wait().unwrap().expect("exited, and still ours to reap");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }

    /// Wait up to `timeout` for `pid` to stop being an unreaped child.
    fn reaped_within(pid: i32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while exited_unreaped(pid).is_some() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    /// Whether the process `pid` is running (not gone, nor a zombie).
    fn running(pid: i32) -> bool {
        let out = std::process::Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output();
        out.is_ok_and(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim_start().starts_with('Z'))
    }

    /// Given a worker whose child exits while it is still `Watched`,
    /// when the worker refreshes,
    /// then it reports Running and leaves the child unreaped, and reaps it
    /// once it is `Reapable` and `ChildExited` arrives.
    #[test]
    fn the_worker_reaps_only_when_the_exit_waiter_lets_it() {
        let shared = Arc::new(Shared::new());
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (pid_tx, pid_rx) = mpsc::channel();
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::spawn(move || {
            let mut term = GhosttyTerminal::spawn(40, 10, 8, 16, Some(sh("exit 4"))).unwrap();
            pid_tx.send(term.child_pid().unwrap() as i32).unwrap();
            worker_loop(&mut term, cmd_rx, &worker_shared, None);
        });
        let pid = pid_rx.recv().unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        while exited_unreaped(pid) != Some(true) {
            assert!(Instant::now() < deadline, "the child never exited");
            std::thread::sleep(Duration::from_millis(10));
        }
        cmd_tx.send(Cmd::ChildExited).unwrap();
        cmd_tx.send(Cmd::PtyReady).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(shared.meta.lock().unwrap().lifecycle, Lifecycle::Running);
        assert_eq!(exited_unreaped(pid), Some(true), "left unreaped");

        *shared.child.lock().unwrap() = ChildState::Reapable;
        cmd_tx.send(Cmd::ChildExited).unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        while shared.meta.lock().unwrap().lifecycle == Lifecycle::Running {
            assert!(Instant::now() < deadline, "the exit was never noticed");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(shared.meta.lock().unwrap().lifecycle, Lifecycle::Exited(Some(4)));
        assert_eq!(*shared.child.lock().unwrap(), ChildState::Reaped(Lifecycle::Exited(Some(4))));
        assert_eq!(exited_unreaped(pid), None, "reaped");
        cmd_tx.send(Cmd::Shutdown).unwrap();
        worker.join().unwrap();
    }

    /// Given a terminal whose child neither reads nor echoes,
    /// when it is dropped,
    /// then the closed PTY hangs the child up, and it is reaped before the
    /// reaper would signal it.
    #[test]
    fn a_dropped_terminals_child_is_hung_up_and_reaped() {
        for script in ["sleep 30", "stty -echo; sleep 30", "stty raw -echo; exec cat"] {
            let term = ThreadedTerminal::spawn(40, 10, 8, 16, Some(sh(script)), None).unwrap();
            let pid = term.child_pid.unwrap() as i32;
            // Let the shell run `stty` before the drop.
            std::thread::sleep(Duration::from_millis(200));
            drop(term);
            assert!(reaped_within(pid, SHUTDOWN_GRACE - REAP_POLL * 10), "{script:?} outlived the hangup");
        }
    }

    /// Given a terminal whose child ignores SIGHUP and neither reads nor
    /// echoes,
    /// when it is dropped,
    /// then its child is killed and reaped once the grace periods pass.
    #[test]
    fn a_dropped_terminals_child_ignoring_sighup_is_killed_and_reaped() {
        let term = ThreadedTerminal::spawn(40, 10, 8, 16, Some(sh("trap '' HUP; stty -echo; sleep 30")), None).unwrap();
        let pid = term.child_pid.unwrap() as i32;
        std::thread::sleep(Duration::from_millis(200));
        drop(term);
        assert!(running(pid), "it survives the hangup");
        assert!(reaped_within(pid, SHUTDOWN_GRACE + KILL_GRACE + Duration::from_secs(1)), "never reaped");
    }

    /// A child of this process, `sh -c script`, leading its own process
    /// group, as a terminal's child does.
    fn group_leader(script: &str) -> std::process::Child {
        use std::os::unix::process::CommandExt;
        std::process::Command::new("/bin/sh").args(["-c", script]).process_group(0).spawn().unwrap()
    }

    /// Given a child exit-watched by nothing but the reaper,
    /// as when the worker has panicked,
    /// when it is reaped as dropped,
    /// then it is hung up and reaped from its pid alone.
    #[test]
    fn the_reaper_needs_no_worker() {
        // Reaped by the code under test.
        let pid = group_leader("sleep 30").id() as i32;
        let shared = Arc::new(Shared::new());
        spawn_exit_waiter(pid as u32, &shared, mpsc::channel().0).unwrap();
        let reaper = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || reap_dropped(&shared, pid))
        };
        assert!(reaped_within(pid, SHUTDOWN_GRACE + KILL_GRACE), "never reaped");
        assert_eq!(*shared.child.lock().unwrap(), ChildState::Reaped(Lifecycle::Signaled(libc::SIGHUP)));
        reaper.join().unwrap();
    }

    /// Given a live child already marked reaped,
    /// when a reap step is due to SIGKILL it,
    /// then it is left alone.
    #[test]
    fn a_reaped_child_is_never_signalled() {
        let mut child = group_leader("sleep 30");
        let pid = child.id() as i32;
        let shared = Shared::new();
        *shared.child.lock().unwrap() = ChildState::Reaped(Lifecycle::Exited(Some(0)));
        assert!(reap_step(&shared, pid, Some(libc::SIGKILL), None), "nothing left to do");
        std::thread::sleep(Duration::from_millis(100));
        assert!(child.try_wait().unwrap().is_none(), "left alone");
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Given a live child whose state lock is poisoned,
    /// when a reap step is due to SIGKILL it,
    /// then it is left alone.
    #[test]
    fn a_poisoned_child_state_leaves_the_child_alone() {
        let mut child = group_leader("sleep 30");
        let pid = child.id() as i32;
        let shared = Arc::new(Shared::new());
        let poisoner = Arc::clone(&shared);
        let _ = std::thread::spawn(move || {
            let _held = poisoner.child.lock().unwrap();
            panic!("poison the child state");
        })
        .join();
        assert!(shared.child.is_poisoned());
        assert!(reap_step(&shared, pid, Some(libc::SIGKILL), None), "nothing left to do");
        std::thread::sleep(Duration::from_millis(100));
        assert!(child.try_wait().unwrap().is_none(), "left alone");
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Given a pid that isn't a child of this process (a grandchild leading
    /// its own group),
    /// when a reap step is due to SIGKILL it,
    /// then it is left alone.
    #[test]
    fn a_pid_that_isnt_a_child_is_never_signalled() {
        use std::io::BufRead;
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "set -m; sleep 30 & echo $!; wait"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let grandchild: i32 = line.trim().parse().unwrap();
        let shared = Shared::new();
        assert!(reap_step(&shared, grandchild, Some(libc::SIGKILL), None), "nothing left to do");
        std::thread::sleep(Duration::from_millis(100));
        assert!(running(grandchild), "left alone");
        // SAFETY: killpg only sends a signal; the grandchild leads its group.
        unsafe {
            libc::killpg(grandchild, libc::SIGKILL);
        }
        let _ = child.wait();
    }

    /// Given an exited child still `Watched`,
    /// when a reap step runs,
    /// then it is left unreaped, and reaped once `Reapable`.
    #[test]
    fn a_watched_child_is_reaped_only_once_reapable() {
        // Reaped by the code under test.
        let pid = group_leader("exit 5").id() as i32;
        let deadline = Instant::now() + Duration::from_secs(4);
        while exited_unreaped(pid) != Some(true) {
            assert!(Instant::now() < deadline, "the child never exited");
            std::thread::sleep(Duration::from_millis(10));
        }
        let shared = Shared::new();
        assert!(!reap_step(&shared, pid, None, None), "waits for the waiter");
        assert_eq!(exited_unreaped(pid), Some(true), "left unreaped");

        *shared.child.lock().unwrap() = ChildState::Reapable;
        assert!(reap_step(&shared, pid, None, None), "reaped");
        assert_eq!(*shared.child.lock().unwrap(), ChildState::Reaped(Lifecycle::Exited(Some(5))));
        assert_eq!(exited_unreaped(pid), None);
    }

    /// Given a live child,
    /// when its exit waiter starts,
    /// then it stays `Watched` until the waiter has seen it exit, and is left
    /// to reap.
    #[test]
    fn the_exit_waiter_marks_the_child_reapable_once_it_exits() {
        let mut child = std::process::Command::new("sh").args(["-c", "sleep 0.3"]).spawn().unwrap();
        let shared = Arc::new(Shared::new());
        let (tx, rx) = mpsc::channel();
        spawn_exit_waiter(child.id(), &shared, tx).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(*shared.child.lock().unwrap(), ChildState::Watched);
        assert!(matches!(rx.recv_timeout(Duration::from_secs(4)), Ok(Cmd::ChildExited)));
        assert_eq!(*shared.child.lock().unwrap(), ChildState::Reapable);
        assert!(child.try_wait().unwrap().is_some(), "the child is left to reap");
    }

    #[test]
    fn a_signal_death_is_reported_with_its_signal() {
        let mut term = ThreadedTerminal::spawn(40, 10, 8, 16, Some(sh("kill -9 $$")), None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        while term.lifecycle() == Lifecycle::Running {
            assert!(Instant::now() < deadline, "the exit was never noticed");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(term.lifecycle(), Lifecycle::Signaled(libc::SIGKILL));
    }

    /// Real-time signals, numbered from 32 on Linux, are reported by number.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_real_time_signal_is_reported_by_number() {
        let mut term = ThreadedTerminal::spawn(40, 10, 8, 16, Some(sh("kill -40 $$")), None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        while term.lifecycle() == Lifecycle::Running {
            assert!(Instant::now() < deadline, "the exit was never noticed");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(term.lifecycle(), Lifecycle::Signaled(40));
    }

    #[test]
    fn the_foreground_group_is_killed_without_the_child() {
        // Job control gives `sleep` its own foreground group in the shell's
        // session; killing it lets the shell carry on.
        let cmd = sh("set -m; sleep 30; printf FG-GONE; sleep 30");
        let mut term = ThreadedTerminal::spawn(40, 10, 8, 16, Some(cmd), None).unwrap();
        let pid = term.child_pid.unwrap() as i32;
        let deadline = Instant::now() + Duration::from_secs(4);
        let group = loop {
            term.resize(40, 10, 8, 16); // wakes the worker to refresh its meta
            std::thread::sleep(Duration::from_millis(20));
            match term.shared.meta.lock().unwrap().foreground {
                Some(g) if g != pid => break g,
                _ => assert!(Instant::now() < deadline, "no foreground job"),
            }
        };
        signal_foreground(pid, Some(group), libc::SIGKILL);
        let deadline = Instant::now() + Duration::from_secs(4);
        while !(0..10).any(|r| row_text(&term.snapshot(), r).contains("FG-GONE")) {
            assert!(Instant::now() < deadline, "the foreground job survived");
            std::thread::sleep(Duration::from_millis(15));
        }
        assert_eq!(term.lifecycle(), Lifecycle::Running, "the shell lives on");
    }

    #[test]
    fn a_stale_foreground_group_outside_the_session_is_not_killed() {
        use std::os::unix::process::CommandExt;
        let term = ThreadedTerminal::spawn(40, 10, 8, 16, Some(sh("sleep 30")), None).unwrap();
        // A process group the shell's session never held.
        let mut other = std::process::Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
        let group = other.id() as i32;
        signal_foreground(term.child_pid.unwrap() as i32, Some(group), libc::SIGKILL);
        std::thread::sleep(Duration::from_millis(100));
        assert!(other.try_wait().unwrap().is_none(), "an unrelated group survives");
        let _ = other.kill();
        let _ = other.wait();
    }

    #[test]
    fn mouse_tracking_and_pastes_go_through_the_worker() {
        use ghostrealm_terminal::{MouseAction, MouseButton, MouseReport, Mods};
        let cmd = sh("stty -echo; printf '\\033[?1000h\\033[?1006h\\033[?2004h'; exec cat -v");
        let mut term = ThreadedTerminal::spawn(40, 10, 8, 16, Some(cmd), None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        while !term.mouse_tracking() {
            assert!(Instant::now() < deadline, "tracking never showed in the meta");
            std::thread::sleep(Duration::from_millis(15));
        }
        term.send_mouse(&MouseReport {
            action: MouseAction::Press,
            button: Some(MouseButton::Left),
            mods: Mods::default(),
            at: (0.5, 0.5),
        });
        term.paste("hi");
        term.write_bytes(b"\r");
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            let g = term.snapshot();
            if (0..10).any(|r| row_text(&g, r) == "^[[<0;1;1M^[[200~hi^[[201~") {
                break;
            }
            assert!(Instant::now() < deadline, "the worker never wrote the mouse report and paste");
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    #[test]
    fn generation_signals_new_output() {
        let mut term =
            ThreadedTerminal::spawn(40, 10, 8, 16, Some(sh("printf X; sleep 2")), None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut changed_once = false;
        while Instant::now() < deadline {
            if term.pump() {
                changed_once = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        assert!(changed_once, "pump() should report a change after output");
        // Consuming the snapshot clears the pending-snapshot signal.
        let mut g = Grid::empty();
        term.snapshot_into(&mut g);
        assert!(!term.needs_snapshot(), "snapshot consumes the pending generation");
    }

    #[test]
    fn a_peek_leaves_the_grid_pending_for_the_paint_path() {
        let mut term = ThreadedTerminal::spawn(40, 10, 8, 16, Some(sh("printf X; sleep 2")), None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !row_text(&term.snapshot(), 0).contains('X') {
            assert!(Instant::now() < deadline, "no output");
            std::thread::sleep(Duration::from_millis(15));
        }
        assert!(term.needs_snapshot(), "a peek doesn't consume the pending grid");
        let mut g = Grid::empty();
        term.snapshot_into(&mut g);
        assert_eq!(row_text(&g, 0), "X");
    }
}
