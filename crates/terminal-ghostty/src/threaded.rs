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
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use ghostrealm_terminal::{Grid, KeyPress, Lifecycle, MouseReport, Pumped, Scroll, TerminalBackend};

use portable_pty::ChildKiller;

use crate::{CommandBuilder, GhosttyTerminal, PtyWaker};

/// How long a dropped terminal's worker gets to see `Shutdown` before the
/// child is sent SIGHUP. A worker blocked writing to a child that isn't
/// reading never sees it; the child's exit fails the write.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// How long after SIGHUP before SIGKILL goes to the child's and the
/// foreground command's process groups (either may ignore SIGHUP).
const KILL_GRACE: Duration = Duration::from_secs(1);

/// Without `waitid` (not unix), how often an otherwise idle worker checks
/// whether the child has exited: nothing else wakes it when the exit brings
/// no output (a background job still holds the PTY) or the reap trails the
/// EOF. Faster once the output has ended, when the exit is due.
#[cfg(not(unix))]
const EXIT_POLL: Duration = Duration::from_millis(500);
#[cfg(not(unix))]
const EXIT_POLL_AFTER_EOF: Duration = Duration::from_millis(50);

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
    worker: Option<JoinHandle<()>>,
    /// Kills the child if the worker doesn't shut down in time.
    killer: Option<Box<dyn ChildKiller + Send + Sync>>,
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
        let shared = Arc::new(Shared {
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
        });
        type Started = Result<(Box<dyn ChildKiller + Send + Sync>, Option<u32>)>;
        let (start_tx, start_rx) = mpsc::channel::<Started>();
        let worker_shared = Arc::clone(&shared);
        // The PTY reader wakes the worker (not the UI) by posting `PtyReady`.
        let waker_tx = cmd_tx.clone();

        let worker = std::thread::Builder::new()
            .name("vt-worker".into())
            .spawn(move || {
                let pty_waker: PtyWaker = {
                    let tx = waker_tx;
                    Box::new(move || {
                        let _ = tx.send(Cmd::PtyReady);
                    })
                };
                let mut term = match GhosttyTerminal::spawn_with_waker(
                    cols,
                    rows,
                    cell_w,
                    cell_h,
                    command,
                    Some(pty_waker),
                ) {
                    Ok(t) => {
                        let _ = start_tx.send(Ok((t.child_killer(), t.child_pid())));
                        t
                    }
                    Err(e) => {
                        let _ = start_tx.send(Err(e));
                        return;
                    }
                };
                worker_loop(&mut term, cmd_rx, &worker_shared, ui_waker.as_ref());
            })
            .map_err(|e| anyhow!("spawn vt-worker: {e}"))?;

        let (killer, child_pid) = match start_rx.recv() {
            Ok(Ok(started)) => started,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(anyhow!("vt-worker died during startup")),
        };
        #[cfg(unix)]
        if let Some(pid) = child_pid {
            let tx = cmd_tx.clone();
            let _ = std::thread::Builder::new().name("vt-exit-waiter".into()).spawn(move || {
                wait_for_exit(pid);
                let _ = tx.send(Cmd::ChildExited);
            });
        }

        Ok(ThreadedTerminal {
            cmd_tx,
            shared,
            last_pumped_gen: 0,
            last_snapshot_gen: 0,
            last_grid: Grid::empty(),
            worker: Some(worker),
            killer: Some(killer),
            child_pid,
        })
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.cmd_tx.send(cmd);
    }
}

/// Dropping returns at once (it runs on the UI thread). A reaper thread waits
/// for the worker; if it is stuck, the reaper sends the child SIGHUP after
/// [`SHUTDOWN_GRACE`], then after [`KILL_GRACE`] SIGKILLs the child's process
/// group and the foreground command's (checked to still be in the child's
/// session). A child already seen to exit isn't signalled (its pid may be
/// reused), but a foreground command left holding the PTY still is.
impl Drop for ThreadedTerminal {
    fn drop(&mut self) {
        self.send(Cmd::Shutdown);
        let (Some(worker), Some(mut killer)) = (self.worker.take(), self.killer.take()) else {
            return;
        };
        let (shared, pid) = (Arc::clone(&self.shared), self.child_pid);
        let reap = move || {
            let finished = |grace: Duration| {
                let deadline = Instant::now() + grace;
                while !worker.is_finished() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                worker.is_finished()
            };
            let exited = || matches!(shared.meta.lock().unwrap().lifecycle, Lifecycle::Exited(_));
            if finished(SHUTDOWN_GRACE) {
                return;
            }
            if !exited() {
                let _ = killer.kill();
            }
            if finished(KILL_GRACE) {
                return;
            }
            let foreground = shared.meta.lock().unwrap().foreground;
            force_kill(pid, !exited(), foreground);
        };
        let _ = std::thread::Builder::new().name("vt-reaper".into()).spawn(reap);
    }
}

/// SIGKILL the process group of the child `pid` (it leads its own), if
/// `kill_child`, and the `foreground` group if it is still in the child's
/// session: the group was read earlier and may since have gone, its id reused.
#[cfg(unix)]
fn force_kill(pid: Option<u32>, kill_child: bool, foreground: Option<i32>) {
    let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 1) else {
        return;
    };
    // SAFETY: getsid only reads a process's session id.
    let in_session = |group: i32| unsafe { libc::getsid(group) } == pid;
    let foreground = foreground.filter(|g| *g > 1 && *g != pid && in_session(*g));
    let child = kill_child.then_some(pid);
    for group in child.into_iter().chain(foreground) {
        // SAFETY: killpg only sends a signal.
        unsafe {
            libc::killpg(group, libc::SIGKILL);
        }
    }
}

/// The killer's TerminateProcess is already forceful.
#[cfg(not(unix))]
fn force_kill(_pid: Option<u32>, _kill_child: bool, _foreground: Option<i32>) {}

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
        // Peek (don't consume) so repeated callers (the agent) keep seeing output.
        let gen = self.shared.generation.load(Ordering::Acquire);
        if let Some(grid) = self.shared.grid.lock().unwrap().clone() {
            self.last_grid = grid;
        }
        self.last_snapshot_gen = gen;
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

/// Block until the child `pid` has exited, leaving it unreaped (the worker
/// reaps it, in `lifecycle`). Returns early if it can't wait (it was reaped
/// already).
#[cfg(unix)]
fn wait_for_exit(pid: u32) {
    let pid: libc::id_t = pid;
    loop {
        // SAFETY: waitid writes only the zeroed siginfo it is given.
        let r = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT)
        };
        if r == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return;
        }
    }
}

/// How long the idle worker waits before checking for the child's exit:
/// never on unix, where a waiter thread posts [`Cmd::ChildExited`].
#[cfg(unix)]
fn exit_poll_after(_term: &GhosttyTerminal, _lifecycle: Lifecycle) -> Option<Duration> {
    None
}

#[cfg(not(unix))]
fn exit_poll_after(term: &GhosttyTerminal, lifecycle: Lifecycle) -> Option<Duration> {
    (lifecycle == Lifecycle::Running)
        .then(|| if term.output_closed() { EXIT_POLL_AFTER_EOF } else { EXIT_POLL })
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
    let mut exit_poll: Option<Duration> = None;
    loop {
        let first = if let Some(timeout) = exit_poll {
            match cmd_rx.recv_timeout(timeout) {
                Ok(m) => Some(m),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match cmd_rx.recv() {
                Ok(m) => Some(m),
                Err(_) => break,
            }
        };
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
            Cmd::ChildExited => {}
            Cmd::Shutdown => shutdown = true,
        };
        if let Some(m) = first {
            apply(m, term);
        }
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
            lifecycle: term.lifecycle(),
            mouse_tracking: term.mouse_tracking(),
            viewport_active: term.viewport_active(),
            foreground: term.foreground_group(),
        };
        exit_poll = exit_poll_after(term, new_meta.lifecycle);
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

        // Gone, or a zombie (the test process never reaps it).
        let alive = || {
            let out = std::process::Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output();
            out.is_ok_and(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim_start().starts_with('Z'))
        };
        let deadline = Instant::now() + SHUTDOWN_GRACE + KILL_GRACE + Duration::from_secs(3);
        while alive() {
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

    #[cfg(unix)]
    #[test]
    fn the_exit_waiter_leaves_the_child_to_reap() {
        let mut child = std::process::Command::new("sh").args(["-c", "sleep 0.2; exit 3"]).spawn().unwrap();
        let start = Instant::now();
        wait_for_exit(child.id());
        assert!(start.elapsed() >= Duration::from_millis(150), "it waited for the exit");
        let status = child.try_wait().unwrap().expect("exited, and still ours to reap");
        assert_eq!(status.code(), Some(3));
    }

    #[cfg(unix)]
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
        force_kill(term.child_pid, false, Some(group));
        let deadline = Instant::now() + Duration::from_secs(4);
        while !(0..10).any(|r| row_text(&term.snapshot(), r).contains("FG-GONE")) {
            assert!(Instant::now() < deadline, "the foreground job survived");
            std::thread::sleep(Duration::from_millis(15));
        }
        assert_eq!(term.lifecycle(), Lifecycle::Running, "the shell lives on");
    }

    #[cfg(unix)]
    #[test]
    fn a_stale_foreground_group_outside_the_session_is_not_killed() {
        use std::os::unix::process::CommandExt;
        let term = ThreadedTerminal::spawn(40, 10, 8, 16, Some(sh("sleep 30")), None).unwrap();
        // A process group the shell's session never held.
        let mut other = std::process::Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
        let group = other.id() as i32;
        force_kill(term.child_pid, false, Some(group));
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
}
