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

use anyhow::{anyhow, Result};
use ghostrealm_terminal::{Grid, KeyPress, Lifecycle, Pumped, Scroll, TerminalBackend};

use crate::{CommandBuilder, GhosttyTerminal, PtyWaker};

/// Wakes the UI event loop after the worker publishes a change.
pub type UiWaker = Arc<dyn Fn() + Send + Sync>;

/// A message to the worker thread.
enum Cmd {
    Key(KeyPress),
    Bytes(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
        cell_w: u32,
        cell_h: u32,
    },
    Scroll(Scroll),
    /// The PTY reader forwarded output; drain + parse it.
    PtyReady,
    Shutdown,
}

/// Cheap terminal state the UI reads without touching the VT.
#[derive(Clone)]
struct Meta {
    title: Option<String>,
    busy: bool,
    lifecycle: Lifecycle,
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
            }),
            generation: AtomicU64::new(0),
        });
        let (start_tx, start_rx) = mpsc::channel::<Result<()>>();
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
                        let _ = start_tx.send(Ok(()));
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

        match start_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(anyhow!("vt-worker died during startup")),
        }

        Ok(ThreadedTerminal {
            cmd_tx,
            shared,
            last_pumped_gen: 0,
            last_snapshot_gen: 0,
            last_grid: Grid::empty(),
            worker: Some(worker),
        })
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.cmd_tx.send(cmd);
    }
}

impl Drop for ThreadedTerminal {
    fn drop(&mut self) {
        self.send(Cmd::Shutdown);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
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
            Cmd::Resize {
                cols,
                rows,
                cell_w,
                cell_h,
            } => term.resize(cols, rows, cell_w, cell_h),
            Cmd::Scroll(s) => term.scroll(s),
            Cmd::PtyReady => pump = true,
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
            lifecycle: term.lifecycle(),
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
