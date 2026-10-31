//! App-side handle to the web host process: spawns it, sends [`Request`]s, and
//! tracks per-browser state from the [`Event`] stream on a reader thread.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ghostrealm_process::inherit_stdio_only;

use crate::frame::FrameReader;
use crate::protocol::{Cursor, Event, MouseAction, PopupKind, Request, PROTOCOL_VERSION};

/// Executable name of the host (and of its macOS bundle, `<name>.app`).
pub const HOST_NAME: &str = "ghostrealm-web-host";

/// Set to a host executable path to override discovery.
pub const HOST_ENV: &str = "GHOSTREALM_WEB_HOST";

/// Set, and neither empty nor `0`, the app's build script skips the web host
/// (`crates/app/build/web_host.rs` reads it the same way; a test checks), so
/// a bundle beside the binaries may be an earlier build's.
pub const NO_WEB_ENV: &str = "GHOSTREALM_NO_WEB";

/// Whether [`NO_WEB_ENV`] is set to skip the web host.
pub fn no_web() -> bool {
    no_web_value(std::env::var(NO_WEB_ENV).ok().as_deref())
}

/// Whether a [`NO_WEB_ENV`] value asks to skip the web host.
pub fn no_web_value(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

/// Set (to anything) to let the host's stderr through instead of discarding it.
pub const DEBUG_ENV: &str = "GHOSTREALM_WEB_DEBUG";

/// How long an eval on a page hidden from Chromium (a person's page off
/// screen, or a person's pop-up the app hasn't shown) can take on a loaded
/// machine. Chromium runs a hidden page's renderer at background priority,
/// which the OS starves: each DevTools step of an eval (a page's first takes
/// five) can then wait seconds, where a visible page's take milliseconds.
pub const HIDDEN_EVAL: Duration = Duration::from_secs(30);

/// Locate the host executable: `$GHOSTREALM_WEB_HOST`, else `explicit` (config),
/// else beside the running executable (`ghostrealm-web-host.app` on macOS, a
/// `ghostrealm-web-host/` directory or bare binary elsewhere).
pub fn find_host(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(HOST_ENV) {
        return Some(PathBuf::from(p));
    }
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let candidates = [
        dir.join(format!("{HOST_NAME}.app/Contents/MacOS/{HOST_NAME}")),
        dir.join(HOST_NAME).join(HOST_NAME),
        dir.join(HOST_NAME),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

/// The latest frame the host announced for a browser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameInfo {
    pub path: String,
    pub slot: u8,
    pub width: u32,
    pub height: u32,
    pub seq: u64,
}

/// What the app knows about one browser, mirrored from host events.
#[derive(Clone, Debug, Default)]
pub struct ViewState {
    pub url: String,
    pub title: String,
    pub loading: bool,
    /// A load has finished at least once (so "not loading" means "loaded").
    pub loaded: bool,
    pub can_back: bool,
    pub can_forward: bool,
    pub cursor: Cursor,
    pub frame: Option<FrameInfo>,
    /// The last failed load, cleared when a new load starts.
    pub error: Option<String>,
    /// Why the page's renderer ended, cleared when a new load (a reload)
    /// starts. Until then the frame is the dead page's last.
    pub gone: Option<String>,
    /// Pop-ups the host refused this page (it had [`MAX_POPUPS`] open),
    /// cleared when a new load starts.
    ///
    /// [`MAX_POPUPS`]: crate::protocol::MAX_POPUPS
    pub popups_refused: u32,
    /// Of those, the ones a person's input asked for that the app hasn't
    /// taken ([`WebHost::take_input_refusals`]), so it can say so: an agent
    /// reads `popups_refused`, a person sees nothing open.
    pub input_refusals: u32,
    /// Bumped on every event for this page but a frame, so a caller can tell
    /// it changed.
    pub version: u64,
    /// Bumped on every frame: a page off screen can paint without anything
    /// on screen changing.
    pub frames: u64,
}

/// A pop-up the host made ([`Event::Popup`]), until the app takes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PopupInfo {
    pub id: u64,
    pub opener: u64,
    pub url: String,
    pub from_input: bool,
    pub kind: PopupKind,
    pub width: u32,
    pub height: u32,
}

impl PopupInfo {
    /// A person's new tab or window: it outlives its opener (the host keeps
    /// it when the opener closes).
    pub fn is_tab(&self) -> bool {
        is_tab(self.from_input, self.kind)
    }
}

/// A page frame the app can hold onto: its shared mapping plus where in it the
/// pixels are. The host may overwrite the slot two frames later; reading then
/// fails rather than returning a torn frame (a newer one is on its way).
#[derive(Clone)]
pub struct SharedFrame {
    reader: Arc<FrameReader>,
    pub slot: u8,
    pub width: u32,
    pub height: u32,
    pub seq: u64,
}

impl SharedFrame {
    /// A copy of the frame's BGRA pixels, or `None` if the host has since
    /// overwritten it.
    pub fn read(&self) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        self.reader
            .read(self.slot, self.seq, self.width, self.height, &mut out)
            .then_some(out)
    }

    /// Identifies the frame's content: changes whenever a new frame arrives,
    /// including after the host replaced its frame file or restarted.
    pub fn version(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.reader.path().hash(&mut h);
        self.seq.hash(&mut h);
        h.finish()
    }
}

#[derive(Default)]
struct Shared {
    ready: bool,
    exited: bool,
    /// Why the host gave up, if it said.
    failure: Option<String>,
    /// The failure is a protocol-version mismatch, which a retry cannot fix.
    mismatch: bool,
    views: HashMap<u64, ViewState>,
    /// `Text` / `Eval` requests awaiting an answer: request id → page id.
    /// Replies to anything else are dropped.
    pending: HashMap<u64, u64>,
    /// Answers to pending requests, keyed by request id.
    replies: HashMap<u64, Event>,
    /// Pages the app asked the host to make, until it forgets them: one the
    /// host closes before any other event about it (a pop-up made again past
    /// its opener's cap) is still reported closed.
    made: std::collections::HashSet<u64>,
    /// Pop-ups not yet taken by their opener's view.
    popups: Vec<PopupInfo>,
    /// A person's tabs whose opener the app forgot before its view took
    /// them, until the app places them.
    orphans: Vec<PopupInfo>,
    /// Pages the host closed and the app hasn't forgotten.
    closed: std::collections::HashSet<u64>,
    /// Of those, the ones that went with a person's refused pop-ups untaken
    /// ([`ViewState::input_refusals`]): a floating pop-up can close before
    /// its view takes them.
    closed_refusals: HashMap<u64, u32>,
    /// Pages the app forgot, and untaken pop-ups the client closed, until the
    /// host reports them closed: their late events are dropped rather than
    /// bring their state back.
    forgotten: std::collections::HashSet<u64>,
}

/// Requests waiting for the writer thread, so a host that stops reading never
/// blocks the caller (the UI thread).
#[derive(Default)]
struct Outbox {
    queue: VecDeque<Request>,
    /// The app is done with the host: write what is queued, then close stdin.
    closed: bool,
    /// A write failed: the host has gone.
    broken: bool,
}

/// Requests that may wait for a host that isn't reading. Past this, mouse
/// moves and wheel steps are dropped and other requests are refused.
const OUTBOX_MAX: usize = 1024;

type Waker = Arc<dyn Fn() + Send + Sync>;

/// Whether the host process has been reaped, set by the reaper [`WebHost`]'s
/// drop starts.
#[derive(Default)]
struct Reaped {
    /// When it was reaped.
    gone: Mutex<Option<Instant>>,
    cv: Condvar,
    /// Told once it has gone, so the GUI can act on it.
    waker: Option<Waker>,
}

impl Reaped {
    fn done(&self) {
        *self.gone.lock().unwrap() = Some(Instant::now());
        self.cv.notify_all();
        if let Some(w) = &self.waker {
            w();
        }
    }
}

/// A host the app hung up on ([`WebHost::hang_up`]), until its process has
/// gone. Until then it may hold the profile, which another host would then
/// report in use.
pub struct Exiting(Arc<Reaped>);

impl Exiting {
    /// Whether the process has gone (exited, or killed for taking too long).
    pub fn is_gone(&self) -> bool {
        self.gone_at().is_some()
    }

    /// When the process went, once it has.
    pub fn gone_at(&self) -> Option<Instant> {
        *self.0.gone.lock().unwrap()
    }

    /// Block until the process has gone (or `timeout`); returns whether it has.
    pub fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut gone = self.0.gone.lock().unwrap();
        while gone.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            gone = self.0.cv.wait_timeout(gone, left).unwrap().0;
        }
        gone.is_some()
    }
}

pub struct WebHost {
    child: Option<Child>,
    outbox: Arc<(Mutex<Outbox>, Condvar)>,
    shared: Arc<(Mutex<Shared>, Condvar)>,
    next_req: AtomicU64,
    readers: HashMap<u64, Arc<FrameReader>>,
    reaped: Arc<Reaped>,
}

impl WebHost {
    /// Start the host at `exe`. `waker` runs (on the reader thread) after every
    /// event, so the GUI can redraw.
    pub fn spawn(exe: &Path, waker: Option<Waker>) -> Result<Self> {
        Self::start(Command::new(exe), waker)
    }

    /// [`Self::spawn`] with `cmd` as the host: tests run a stand-in from
    /// `/bin/sh -c` rather than writing a script, which macOS checks on its
    /// first run for an unbounded time.
    fn start(mut cmd: Command, waker: Option<Waker>) -> Result<Self> {
        let stderr = if std::env::var_os(DEBUG_ENV).is_some() {
            Stdio::inherit()
        } else {
            Stdio::null()
        };
        let mut child = inherit_stdio_only(&mut cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .with_context(|| format!("spawn web host {}", Path::new(cmd.get_program()).display()))?;
        let stdin = child.stdin.take().context("web host stdin")?;
        let stdout = child.stdout.take().context("web host stdout")?;
        let reaped = Arc::new(Reaped { waker: waker.clone(), ..Reaped::default() });
        let shared: Arc<(Mutex<Shared>, Condvar)> = Arc::default();
        let reader_shared = shared.clone();
        let outbox: Arc<(Mutex<Outbox>, Condvar)> = Arc::default();
        let reader_outbox = outbox.clone();
        std::thread::Builder::new()
            .name("web-host-events".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    let Ok(ev) = serde_json::from_str::<Event>(&line) else {
                        continue;
                    };
                    // Pop-ups no view will take are closed, not left running.
                    for id in apply_event(&reader_shared, ev) {
                        enqueue(&reader_outbox, &Request::Close { id });
                    }
                    if let Some(w) = &waker {
                        w();
                    }
                }
                let (lock, cv) = &*reader_shared;
                lock.lock().unwrap().exited = true;
                cv.notify_all();
                if let Some(w) = &waker {
                    w();
                }
            })
            .context("spawn web host reader")?;
        let writer_outbox = outbox.clone();
        std::thread::Builder::new()
            .name("web-host-requests".into())
            .spawn(move || write_requests(stdin, &writer_outbox))
            .context("spawn web host writer")?;
        Ok(WebHost {
            child: Some(child),
            outbox,
            shared,
            next_req: AtomicU64::new(1),
            readers: HashMap::new(),
            reaped,
        })
    }

    /// Ask the host to exit, as dropping it does, and keep track of when its
    /// process has gone.
    pub fn hang_up(self) -> Exiting {
        Exiting(self.reaped.clone())
    }

    /// Queue a request for the host; never blocks on the pipe. A mouse move
    /// replaces one still queued for the same page. Returns false if the host
    /// has gone away, or isn't reading and the queue is full (a dropped mouse
    /// move or wheel step still returns true).
    pub fn send(&self, req: &Request) -> bool {
        // Known before the host can answer: it may close the page at once.
        let made = match req {
            Request::Create { id, .. } => Some(*id).filter(|id| self.shared.0.lock().unwrap().made.insert(*id)),
            _ => None,
        };
        let sent = enqueue(&self.outbox, req);
        if let (false, Some(id)) = (sent, made) {
            self.shared.0.lock().unwrap().made.remove(&id);
        }
        sent
    }

    /// The pages the host reported closed and the app hasn't forgotten.
    pub fn closed_pages(&self) -> Vec<u64> {
        self.shared.0.lock().unwrap().closed.iter().copied().collect()
    }

    /// Whether the host process is still running.
    pub fn is_alive(&self) -> bool {
        !self.shared.0.lock().unwrap().exited
    }

    /// How the host process ended, once it has (`None` while it runs).
    pub fn exit_status(&mut self) -> Option<std::process::ExitStatus> {
        self.child.as_mut().and_then(|c| c.try_wait().ok().flatten())
    }

    /// The host's process id.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// Whether the host has reported CEF initialised.
    pub fn is_ready(&self) -> bool {
        self.shared.0.lock().unwrap().ready
    }

    /// Why the host can't be used: it gave up (and is exiting), or it speaks
    /// another protocol version.
    pub fn failure(&self) -> Option<String> {
        self.shared.0.lock().unwrap().failure.clone()
    }

    /// Whether the host speaks another protocol version (see
    /// [`failure`](Self::failure)). Respawning the same bundle cannot fix it.
    pub fn protocol_mismatch(&self) -> bool {
        self.shared.0.lock().unwrap().mismatch
    }

    /// Block until the host reports CEF is initialised (or `timeout`, exit, or
    /// a [`failure`](Self::failure)).
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        self.wait_until(timeout, |s| s.ready || s.failure.is_some())
            .is_some_and(|s| s.ready)
    }

    /// A snapshot of browser `id`'s state.
    pub fn view(&self, id: u64) -> Option<ViewState> {
        self.shared.0.lock().unwrap().views.get(&id).cloned()
    }

    /// Drop local state for a closed browser.
    pub fn forget(&mut self, id: u64) {
        let mut s = self.shared.0.lock().unwrap();
        s.views.remove(&id);
        s.closed_refusals.remove(&id);
        s.made.remove(&id);
        if !s.closed.remove(&id) {
            s.forgotten.insert(id);
        }
        s.popups.retain(|p| p.id != id);
        s.orphans.retain(|p| p.id != id);
        let untaken = drop_popups_of(&mut s, id);
        drop(s);
        for id in untaken {
            self.send(&Request::Close { id });
        }
        self.readers.remove(&id);
    }

    /// Whether a person's tab whose opener the app forgot waits to be placed
    /// (see [`take_orphan_tabs`](Self::take_orphan_tabs)).
    pub fn has_orphan_tabs(&self) -> bool {
        !self.shared.0.lock().unwrap().orphans.is_empty()
    }

    /// The tabs a person opened whose opener the app forgot before its view
    /// took them, oldest first: they outlive it, so the app places them.
    pub fn take_orphan_tabs(&self) -> Vec<PopupInfo> {
        std::mem::take(&mut self.shared.0.lock().unwrap().orphans)
    }

    /// Hand back person's tab `p`, taken by its opener's view, which went
    /// before the app placed the tab: [`take_orphan_tabs`] returns it.
    ///
    /// [`take_orphan_tabs`]: Self::take_orphan_tabs
    pub fn orphan_tab(&self, p: PopupInfo) {
        self.shared.0.lock().unwrap().orphans.push(p);
    }

    /// Whether the host closed page `id` (see [`Event::Closed`]).
    pub fn is_closed(&self, id: u64) -> bool {
        self.shared.0.lock().unwrap().closed.contains(&id)
    }

    /// The pop-ups page `opener` opened since last asked, oldest first (some
    /// may have closed since: see [`is_closed`](Self::is_closed)).
    pub fn take_popups(&self, opener: u64) -> Vec<PopupInfo> {
        let mut s = self.shared.0.lock().unwrap();
        let (mine, rest) = std::mem::take(&mut s.popups).into_iter().partition(|p| p.opener == opener);
        s.popups = rest;
        mine
    }

    /// How many pop-ups a person asked page `id` for that the host refused
    /// since last asked.
    pub fn take_input_refusals(&self, id: u64) -> u32 {
        let mut s = self.shared.0.lock().unwrap();
        let closed = s.closed_refusals.remove(&id).unwrap_or(0);
        closed + s.views.get_mut(&id).map_or(0, |v| std::mem::take(&mut v.input_refusals))
    }

    /// The latest frame of `id`.
    pub fn frame(&mut self, id: u64) -> Option<SharedFrame> {
        let info = self.view(id)?.frame?;
        let stale = self.readers.get(&id).is_none_or(|r| r.path() != info.path);
        if stale {
            // The host may already have replaced this file; skip to the next frame.
            let reader = FrameReader::open(&info.path).ok()?;
            self.readers.insert(id, Arc::new(reader));
        }
        let frame = SharedFrame {
            reader: self.readers.get(&id)?.clone(),
            slot: info.slot,
            width: info.width,
            height: info.height,
            seq: info.seq,
        };
        frame
            .reader
            .holds(frame.slot, frame.width, frame.height)
            .then_some(frame)
    }

    /// Note that a navigation was just requested for `id`, so [`wait_loaded`]
    /// waits for it to finish rather than returning on the previous page.
    ///
    /// [`wait_loaded`]: Self::wait_loaded
    pub fn expect_load(&self, id: u64) {
        if let Some(v) = self.shared.0.lock().unwrap().views.get_mut(&id) {
            v.loaded = false;
        }
    }

    /// Block until browser `id` has finished loading (or `timeout`).
    pub fn wait_loaded(&self, id: u64, timeout: Duration) -> bool {
        self.wait_until(timeout, |s| {
            s.views.get(&id).is_some_and(|v| v.loaded && !v.loading)
        })
        .is_some_and(|s| s.views.get(&id).is_some_and(|v| v.loaded && !v.loading))
    }

    /// The main frame's visible text.
    pub fn text(&self, id: u64, timeout: Duration) -> Option<String> {
        let req = self.expect_reply(id);
        if !self.send(&Request::Text { id, req }) {
            self.forget_reply(req);
            return None;
        }
        match self.wait_reply(req, timeout)? {
            Event::Text { text, .. } => Some(text),
            _ => None,
        }
    }

    /// Evaluate `js` in the main frame, isolated from the page's scripts (see
    /// [`eval_wrapper`]). `Ok` carries the JSON-encoded value.
    ///
    /// [`eval_wrapper`]: crate::protocol::eval_wrapper
    pub fn eval(&self, id: u64, js: &str, timeout: Duration) -> Result<String, String> {
        self.eval_as(id, js, false, timeout)
    }

    /// [`eval`](Self::eval), as a user gesture when `gesture` (see
    /// [`Request::Eval`]).
    pub fn eval_as(&self, id: u64, js: &str, gesture: bool, timeout: Duration) -> Result<String, String> {
        let req = self.expect_reply(id);
        if !self.send(&Request::Eval {
            id,
            req,
            js: js.to_string(),
            gesture,
        }) {
            self.forget_reply(req);
            return Err("web host is not running".into());
        }
        match self.wait_reply(req, timeout) {
            Some(Event::EvalResult {
                ok: true, value, ..
            }) => Ok(value),
            Some(Event::EvalResult { value, .. }) => Err(value),
            _ => Err(format!("no result within {}s", timeout.as_secs_f32())),
        }
    }

    /// A fresh request id whose reply (about page `id`) will be kept.
    fn expect_reply(&self, id: u64) -> u64 {
        let req = self.next_req.fetch_add(1, Ordering::Relaxed);
        self.shared.0.lock().unwrap().pending.insert(req, id);
        req
    }

    fn forget_reply(&self, req: u64) {
        let mut s = self.shared.0.lock().unwrap();
        s.pending.remove(&req);
        s.replies.remove(&req);
    }

    /// The reply to `req`, or `None` on timeout or exit. Either way the request
    /// is no longer pending, so a late reply is dropped.
    fn wait_reply(&self, req: u64, timeout: Duration) -> Option<Event> {
        let mut s = self.wait_until(timeout, |s| s.replies.contains_key(&req))?;
        s.pending.remove(&req);
        s.replies.remove(&req)
    }

    /// Wait for `done` (or exit/timeout), returning the locked state.
    fn wait_until(
        &self,
        timeout: Duration,
        done: impl Fn(&Shared) -> bool,
    ) -> Option<std::sync::MutexGuard<'_, Shared>> {
        let deadline = Instant::now() + timeout;
        let (lock, cv) = &*self.shared;
        let mut s = lock.lock().ok()?;
        while !done(&s) && !s.exited {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            s = cv.wait_timeout(s, left).ok()?.0;
        }
        Some(s)
    }
}

impl Drop for WebHost {
    fn drop(&mut self) {
        // EOF on stdin (once the queue is written) asks the host to close its
        // browsers and exit; reap it off this thread so a slow shutdown never
        // blocks the caller.
        let (lock, cv) = &*self.outbox;
        lock.lock().unwrap().closed = true;
        cv.notify_one();
        let reaped = self.reaped.clone();
        let Some(mut child) = self.child.take() else {
            reaped.done();
            return;
        };
        let reaper = reaped.clone();
        let spawned = std::thread::Builder::new()
            .name("web-host-reap".into())
            .spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    if let Ok(Some(_)) = child.try_wait() {
                        reaper.done();
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _ = child.kill();
                let _ = child.wait();
                reaper.done();
            });
        // Nothing would report it gone: don't leave a waiter waiting forever.
        if spawned.is_err() {
            reaped.done();
        }
    }
}

/// Queue `req` for the writer thread (see [`WebHost::send`]).
fn enqueue(outbox: &(Mutex<Outbox>, Condvar), req: &Request) -> bool {
    let (lock, cv) = outbox;
    let mut o = lock.lock().unwrap();
    if o.broken || o.closed {
        return false;
    }
    let (moving, lossy) = match req {
        Request::Mouse { id, action, .. } => (
            (*action == MouseAction::Move).then_some(*id),
            matches!(action, MouseAction::Move | MouseAction::Wheel { .. }),
        ),
        _ => (None, false),
    };
    if let (Some(id), Some(Request::Mouse { id: last, action: MouseAction::Move, .. })) =
        (moving, o.queue.back())
    {
        if *last == id {
            *o.queue.back_mut().expect("checked") = req.clone();
            return true;
        }
    }
    if o.queue.len() >= OUTBOX_MAX {
        return lossy;
    }
    o.queue.push_back(req.clone());
    cv.notify_one();
    true
}

/// The writer thread: write queued requests to the host's stdin until the app
/// is done with it (then close it) or the host goes away.
fn write_requests(mut stdin: ChildStdin, outbox: &(Mutex<Outbox>, Condvar)) {
    let (lock, cv) = outbox;
    let mut buf = Vec::new();
    loop {
        {
            let mut o = lock.lock().unwrap();
            while o.queue.is_empty() && !o.closed {
                o = cv.wait(o).unwrap();
            }
            if o.queue.is_empty() {
                return;
            }
            buf.clear();
            for req in o.queue.drain(..) {
                if serde_json::to_writer(&mut buf, &req).is_ok() {
                    buf.push(b'\n');
                }
            }
        }
        if stdin.write_all(&buf).and_then(|_| stdin.flush()).is_err() {
            let mut o = lock.lock().unwrap();
            o.broken = true;
            o.queue.clear();
            return;
        }
    }
}

/// Apply `ev`; returns the pop-ups to close because no view will take them
/// (their opener was forgotten).
fn apply_event(shared: &(Mutex<Shared>, Condvar), ev: Event) -> Vec<u64> {
    let (lock, cv) = shared;
    let mut s = lock.lock().unwrap();
    let late = match &ev {
        Event::Closed { id } if s.forgotten.remove(id) => Some(drop_popups_of(&mut s, *id)),
        Event::Popup { id, opener, from_input, kind, .. }
            if s.forgotten.contains(opener) && !is_tab(*from_input, *kind) =>
        {
            s.forgotten.insert(*id);
            Some(vec![*id])
        }
        ev => page_of(ev).is_some_and(|id| s.forgotten.contains(&id)).then(Vec::new),
    };
    if let Some(orphans) = late {
        return orphans;
    }
    if let Some(id) = page_of(&ev) {
        let v = s.views.entry(id).or_default();
        if matches!(ev, Event::Frame { .. }) {
            v.frames += 1;
        } else {
            v.version += 1;
        }
    }
    match ev {
        Event::Ready { protocol } if protocol == PROTOCOL_VERSION => s.ready = true,
        Event::Ready { protocol } => {
            s.mismatch = true;
            s.failure = Some(format!(
                "the web host bundle is out of date (protocol {protocol}, this ghostrealm \
                 speaks {PROTOCOL_VERSION}); rebuild it with `cargo build`"
            ));
        }
        Event::Frame {
            id,
            path,
            slot,
            width,
            height,
            seq,
        } => {
            s.views.entry(id).or_default().frame = Some(FrameInfo {
                path,
                slot,
                width,
                height,
                seq,
            });
        }
        Event::Address { id, url } => s.views.entry(id).or_default().url = url,
        Event::Title { id, title } => s.views.entry(id).or_default().title = title,
        Event::Loading {
            id,
            loading,
            can_back,
            can_forward,
        } => {
            let v = s.views.entry(id).or_default();
            if loading && !v.loading {
                v.error = None;
                v.gone = None;
                v.popups_refused = 0;
            }
            v.loading = loading;
            v.loaded |= !loading;
            v.can_back = can_back;
            v.can_forward = can_forward;
        }
        Event::Cursor { id, cursor } => s.views.entry(id).or_default().cursor = cursor,
        Event::LoadError { id, url, error } => {
            s.views.entry(id).or_default().error = Some(format!("{error} ({url})"));
        }
        Event::RendererGone { id, error } => s.views.entry(id).or_default().gone = Some(error),
        Event::Popup { id, opener, url, from_input, kind, width, height } => {
            s.views.entry(id).or_default().url.clone_from(&url);
            let p = PopupInfo { id, opener, url, from_input, kind, width, height };
            // A person's tab whose opener was forgotten (any other pop-up of
            // it was closed above).
            if s.forgotten.contains(&opener) {
                s.orphans.push(p);
            } else {
                s.popups.push(p);
            }
        }
        Event::PopupRefused { opener, from_input, .. } => {
            let v = s.views.entry(opener).or_default();
            v.popups_refused += 1;
            v.input_refusals += u32::from(from_input);
        }
        // A person's tab no view took yet closed: none will now.
        Event::Closed { id } if s.orphans.iter().any(|p| p.id == id) => {
            s.orphans.retain(|p| p.id != id);
            s.views.remove(&id);
        }
        // A page that closed itself may still have a view: that view takes
        // the pop-ups it opened (a tab can outlive it).
        Event::Closed { id } => {
            let view = s.views.remove(&id);
            let made = s.made.remove(&id);
            let known = made || view.is_some() || s.popups.iter().any(|p| p.id == id);
            if known {
                s.closed.insert(id);
                if let Some(n) = view.map(|v| v.input_refusals).filter(|n| *n > 0) {
                    s.closed_refusals.insert(id, n);
                }
            }
        }
        Event::Failed { error } => s.failure = Some(error),
        reply @ (Event::Text { id, req, .. } | Event::EvalResult { id, req, .. }) => {
            // Only a reply about the page the request named; a page that answers
            // for another is ignored and the real answer still lands.
            if s.pending.get(&req) == Some(&id) {
                s.pending.remove(&req);
                s.replies.insert(req, reply);
            }
        }
    }
    cv.notify_all();
    Vec::new()
}

/// Forget the pop-ups the forgotten page `opener` opened that no view took
/// (none will now): a person's tabs still open are kept for the app to
/// place; the other pop-ups still open are returned, to close.
fn drop_popups_of(s: &mut Shared, opener: u64) -> Vec<u64> {
    let (gone, keep): (Vec<PopupInfo>, Vec<PopupInfo>) =
        std::mem::take(&mut s.popups).into_iter().partition(|p| p.opener == opener);
    s.popups = keep;
    let mut open = Vec::new();
    for p in gone {
        if s.closed.remove(&p.id) {
            s.closed_refusals.remove(&p.id);
            s.views.remove(&p.id);
        } else if p.is_tab() {
            s.orphans.push(p);
        } else {
            s.views.remove(&p.id);
            s.forgotten.insert(p.id);
            open.push(p.id);
        }
    }
    open
}

/// Whether a pop-up is a person's new tab or window (see [`PopupInfo::is_tab`]).
fn is_tab(from_input: bool, kind: PopupKind) -> bool {
    from_input && kind != PopupKind::Popup
}

/// The page an event updates the state of (not replies, which answer requests).
fn page_of(ev: &Event) -> Option<u64> {
    match ev {
        Event::Frame { id, .. }
        | Event::Address { id, .. }
        | Event::Title { id, .. }
        | Event::Loading { id, .. }
        | Event::Cursor { id, .. }
        | Event::LoadError { id, .. }
        | Event::RendererGone { id, .. }
        | Event::Popup { id, .. }
        | Event::PopupRefused { opener: id, .. } => Some(*id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host that starts but never reads its stdin fills the pipe; sending
    /// still returns at once, coalescing moves and refusing past the cap.
    #[cfg(unix)]
    #[test]
    fn sending_to_a_host_that_doesnt_read_never_blocks() {
        let mut sleep = Command::new("sleep");
        sleep.arg("30");
        let host = WebHost::start(sleep, None).unwrap();
        let mouse = |x, action| Request::Mouse {
            id: 1,
            x,
            y: 0,
            mods: 0,
            action,
        };
        let key = Request::Eval {
            id: 1,
            req: 0,
            js: "x".repeat(1000),
            gesture: false,
        };
        // More than a pipe holds: once the writer thread takes it, it is
        // stuck on that write for good, and takes nothing more from the
        // queue (otherwise it may first run after the loop below and take
        // the whole queue).
        let huge = Request::Eval { id: 1, req: 0, js: "x".repeat(4 << 20), gesture: false };
        assert!(host.send(&huge));
        let until = Instant::now() + Duration::from_secs(5);
        while !host.outbox.0.lock().unwrap().queue.is_empty() {
            assert!(Instant::now() < until, "the writer never took the first request");
            std::thread::sleep(Duration::from_millis(1));
        }
        let start = Instant::now();
        // Far more than the queue holds.
        for i in 0..5_000 {
            assert!(host.send(&key) || i > 100, "refused before the queue could fill");
            host.send(&mouse(i, MouseAction::Move));
        }
        assert!(start.elapsed() < Duration::from_secs(2), "send blocked");
        let queued = host.outbox.0.lock().unwrap().queue.len();
        assert_eq!(queued, OUTBOX_MAX, "the queue stays bounded");
        assert!(!host.send(&key), "a full queue refuses requests");
        assert!(host.send(&mouse(0, MouseAction::Wheel { dx: 0, dy: 1 })), "wheel steps are dropped");
        assert!(host.send(&mouse(7, MouseAction::Move)), "moves are dropped or coalesced");
        let pid = host.pid().unwrap();
        drop(host);
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }

    /// Given a host that closed its stdout at once and, once its stdin
    /// closes, takes a moment to exit;
    /// when the app hangs up on it;
    /// then it is not gone at once, and once its process has gone it is
    /// reported gone, a waiter returns, and the waker is told.
    #[cfg(unix)]
    #[test]
    fn a_host_hung_up_on_is_reported_gone_once_its_process_has_gone() {
        use std::sync::atomic::AtomicUsize;
        let mut sh = Command::new("/bin/sh");
        sh.args(["-c", "exec 1>&-; cat > /dev/null; sleep 0.5"]);
        let woken = Arc::new(AtomicUsize::new(0));
        let w = woken.clone();
        let host = WebHost::start(sh, Some(Arc::new(move || {
            w.fetch_add(1, Ordering::Relaxed);
        })))
        .unwrap();
        let pid = host.pid().unwrap();
        // Its stdout's end wakes too: let that pass first.
        let until = Instant::now() + Duration::from_secs(2);
        while host.is_alive() {
            assert!(Instant::now() < until, "its stdout never closed");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(50));
        let woken_before = woken.load(Ordering::Relaxed);
        let exiting = host.hang_up();
        assert!(!exiting.is_gone(), "it is still exiting");
        assert!(exiting.wait(Duration::from_secs(4)), "it went before the reaper's kill");
        assert!(exiting.is_gone());
        let alive = Command::new("kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status();
        assert!(!alive.is_ok_and(|s| s.success()), "its process has gone");
        let until = Instant::now() + Duration::from_secs(2);
        while woken.load(Ordering::Relaxed) == woken_before {
            assert!(Instant::now() < until, "the waker was not told");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_queued_mouse_move_is_replaced_by_the_next() {
        let outbox: (Mutex<Outbox>, Condvar) = Default::default();
        let host = WebHost {
            child: None,
            outbox: Arc::new(outbox),
            shared: Arc::default(),
            next_req: AtomicU64::new(1),
            readers: HashMap::new(),
            reaped: Arc::default(),
        };
        let mv = |id, x| Request::Mouse {
            id,
            x,
            y: 0,
            mods: 0,
            action: MouseAction::Move,
        };
        for x in 0..10 {
            assert!(host.send(&mv(1, x)));
        }
        assert!(host.send(&mv(2, 0)));
        assert!(host.send(&mv(2, 5)));
        let q: Vec<Request> = host.outbox.0.lock().unwrap().queue.iter().cloned().collect();
        assert_eq!(q, vec![mv(1, 9), mv(2, 5)]);
    }

    fn eval_result(id: u64, req: u64, value: &str) -> Event {
        Event::EvalResult {
            id,
            req,
            ok: true,
            value: value.into(),
        }
    }

    #[test]
    fn a_host_speaking_another_protocol_is_refused() {
        let shared: (Mutex<Shared>, Condvar) = Default::default();
        apply_event(&shared, Event::Ready { protocol: 0 });
        {
            let s = shared.0.lock().unwrap();
            assert!(!s.ready);
            let failure = s.failure.as_deref().unwrap_or_default();
            assert!(failure.contains("out of date") && failure.contains("cargo build"), "{failure}");
        }
        let shared: (Mutex<Shared>, Condvar) = Default::default();
        apply_event(&shared, Event::Ready { protocol: PROTOCOL_VERSION });
        let s = shared.0.lock().unwrap();
        assert!(s.ready && s.failure.is_none());
    }

    #[test]
    fn a_dead_renderer_is_reported_until_the_next_load() {
        let shared: (Mutex<Shared>, Condvar) = Default::default();
        let gone = |s: &(Mutex<Shared>, Condvar)| s.0.lock().unwrap().views[&1].gone.clone();
        let loading = |loading| Event::Loading { id: 1, loading, can_back: false, can_forward: false };
        apply_event(&shared, loading(true));
        apply_event(&shared, loading(false));
        apply_event(&shared, Event::RendererGone { id: 1, error: "the page's renderer crashed".into() });
        assert_eq!(gone(&shared).as_deref(), Some("the page's renderer crashed"));
        apply_event(&shared, loading(true));
        assert_eq!(gone(&shared), None, "a reload clears it");
    }

    fn popup(id: u64, opener: u64, from_input: bool, kind: PopupKind) -> Event {
        Event::Popup { id, opener, url: String::new(), from_input, kind, width: 1, height: 1 }
    }

    /// Given page 1 opened a person's tab and page 2 an agent's pop-up (11),
    /// neither taken by a view;
    /// when page 1 closes itself, and the app forgets page 2, after which
    /// page 2's second agent's pop-up (12) arrives;
    /// then page 1's view still takes its tab, while 11 and 12 are closed,
    /// their late events are dropped, and nothing is kept for them once the
    /// host reports them closed.
    #[test]
    fn untaken_popups_wait_for_their_opener_or_close_with_it() {
        let mut host = WebHost {
            child: None,
            outbox: Arc::default(),
            shared: Arc::default(),
            next_req: AtomicU64::new(1),
            readers: HashMap::new(),
            reaped: Arc::default(),
        };
        let shared = host.shared.clone();
        apply_event(&shared, Event::Title { id: 1, title: "t".into() });
        assert!(apply_event(&shared, popup(10, 1, true, PopupKind::Tab)).is_empty());
        apply_event(&shared, Event::Closed { id: 99 });
        assert!(!shared.0.lock().unwrap().closed.contains(&99), "a page the app never had");

        // The opener closed itself: its view still takes the pop-up.
        apply_event(&shared, Event::Closed { id: 1 });
        assert_eq!(host.take_popups(1).len(), 1);

        // A forgotten opener's untaken pop-ups close, late ones too.
        apply_event(&shared, Event::Title { id: 2, title: "t".into() });
        apply_event(&shared, popup(11, 2, false, PopupKind::Tab));
        host.forget(2);
        assert_eq!(apply_event(&shared, popup(12, 2, false, PopupKind::Tab)), [12]);
        let queued: Vec<Request> = host.outbox.0.lock().unwrap().queue.iter().cloned().collect();
        assert_eq!(queued, [Request::Close { id: 11 }]);
        assert!(apply_event(&shared, Event::Title { id: 11, title: "late".into() }).is_empty());
        assert!(host.view(11).is_none(), "its late events are dropped");
        for id in [2, 11, 12] {
            apply_event(&shared, Event::Closed { id });
        }
        let s = shared.0.lock().unwrap();
        assert!(s.popups.is_empty() && s.forgotten.is_empty(), "nothing kept");
        assert_eq!(s.closed.iter().copied().collect::<Vec<_>>(), [1], "the opener the app still holds");
    }

    /// Given page 2 opened, untaken by a view: a person's tab (11), an
    /// agent's pop-up (13), a person's pop-up window (14), and a person's tab
    /// that has since closed (16);
    /// when the app forgets page 2, after which page 2's person's new window
    /// (12) and background tab (15) arrive, and 15 closes;
    /// then 13 and 14 are closed while 11 and 12 keep running, with their
    /// late events, and are handed to the app once, oldest first; 15 and 16
    /// are not handed over, and nothing is kept for any of them.
    #[test]
    fn a_persons_tab_outlives_a_forgotten_opener_for_the_app_to_place() {
        let mut host = WebHost {
            child: None,
            outbox: Arc::default(),
            shared: Arc::default(),
            next_req: AtomicU64::new(1),
            readers: HashMap::new(),
            reaped: Arc::default(),
        };
        let shared = host.shared.clone();
        apply_event(&shared, Event::Title { id: 2, title: "t".into() });
        apply_event(&shared, popup(11, 2, true, PopupKind::Tab));
        apply_event(&shared, popup(13, 2, false, PopupKind::Tab));
        apply_event(&shared, popup(14, 2, true, PopupKind::Popup));
        apply_event(&shared, popup(16, 2, true, PopupKind::Tab));
        apply_event(&shared, Event::Closed { id: 16 });
        host.forget(2);
        assert!(apply_event(&shared, popup(12, 2, true, PopupKind::Window)).is_empty(), "a late tab isn't closed");
        assert!(apply_event(&shared, popup(15, 2, true, PopupKind::BackgroundTab)).is_empty());
        apply_event(&shared, Event::Title { id: 12, title: "late".into() });
        apply_event(&shared, Event::Closed { id: 15 });
        let queued: Vec<Request> = host.outbox.0.lock().unwrap().queue.iter().cloned().collect();
        assert_eq!(queued, [Request::Close { id: 13 }, Request::Close { id: 14 }]);

        let tabs = host.take_orphan_tabs();
        assert_eq!(tabs.iter().map(|p| (p.id, p.kind)).collect::<Vec<_>>(), [(11, PopupKind::Tab), (12, PopupKind::Window)]);
        assert_eq!(host.view(12).map(|v| v.title).as_deref(), Some("late"), "its events are kept");
        assert!(host.take_orphan_tabs().is_empty(), "handed over once");
        for id in [2, 13, 14] {
            apply_event(&shared, Event::Closed { id });
        }
        let s = shared.0.lock().unwrap();
        assert!(s.popups.is_empty() && s.forgotten.is_empty() && s.closed.is_empty(), "nothing kept");
        assert!(!s.views.contains_key(&15) && !s.views.contains_key(&16));
    }

    #[test]
    fn refused_popups_are_counted_until_the_next_load() {
        let shared: (Mutex<Shared>, Condvar) = Default::default();
        let refused = || Event::PopupRefused { opener: 1, url: String::new(), from_input: false };
        let loading = |loading| Event::Loading { id: 1, loading, can_back: false, can_forward: false };
        apply_event(&shared, refused());
        apply_event(&shared, refused());
        assert_eq!(shared.0.lock().unwrap().views[&1].popups_refused, 2);
        apply_event(&shared, loading(true));
        assert_eq!(shared.0.lock().unwrap().views[&1].popups_refused, 0);
    }

    /// The refusals a person's input caused are handed to the app once; an
    /// agent's are only counted.
    #[test]
    fn a_persons_refused_popups_are_taken_once() {
        let host = WebHost {
            child: None,
            outbox: Arc::default(),
            shared: Arc::default(),
            next_req: AtomicU64::new(1),
            readers: HashMap::new(),
            reaped: Arc::default(),
        };
        let refused = |from_input| Event::PopupRefused { opener: 1, url: String::new(), from_input };
        apply_event(&host.shared, refused(false));
        assert_eq!(host.take_input_refusals(1), 0, "an agent's");
        apply_event(&host.shared, refused(true));
        apply_event(&host.shared, refused(true));
        assert_eq!(host.take_input_refusals(1), 2);
        assert_eq!(host.take_input_refusals(1), 0, "taken once");
        assert_eq!(host.view(1).unwrap().popups_refused, 3, "all are counted");
        assert_eq!(host.take_input_refusals(2), 0, "an unknown page");

        // A page that closes before they are taken keeps them until taken,
        // or until the app forgets it.
        let mut host = host;
        apply_event(&host.shared, Event::Title { id: 3, title: "t".into() });
        for id in [1, 3] {
            apply_event(&host.shared, Event::PopupRefused { opener: id, url: String::new(), from_input: true });
            apply_event(&host.shared, Event::Closed { id });
        }
        assert_eq!(host.take_input_refusals(1), 1);
        assert_eq!(host.take_input_refusals(1), 0, "taken once");
        host.forget(3);
        assert_eq!(host.take_input_refusals(3), 0, "forgotten");
        assert!(host.shared.0.lock().unwrap().closed_refusals.is_empty());

        // A closed pop-up's are dropped once the app forgets its opener.
        apply_event(&host.shared, popup(5, 1, false, PopupKind::Popup));
        apply_event(&host.shared, Event::PopupRefused { opener: 5, url: String::new(), from_input: true });
        apply_event(&host.shared, Event::Closed { id: 5 });
        assert!(host.shared.0.lock().unwrap().closed_refusals.contains_key(&5));
        host.forget(1);
        assert!(host.shared.0.lock().unwrap().closed_refusals.is_empty(), "dropped with its opener");
    }

    /// Given a pop-up made again that the host closes before reporting
    /// anything else about it (refused past its opener's cap);
    /// when its `Closed` arrives as soon as the `Create` is queued (the
    /// writer and reader run on their own threads);
    /// then it is closed. A `Create` the host never got is not awaited.
    #[test]
    fn a_page_closed_before_any_other_event_is_closed() {
        let host = WebHost {
            child: None,
            outbox: Arc::default(),
            shared: Arc::default(),
            next_req: AtomicU64::new(1),
            readers: HashMap::new(),
            reaped: Arc::default(),
        };
        let create = |id| Request::Create {
            id,
            url: "https://pop.test/".into(),
            width: 200,
            height: 100,
            scale: 1.0,
            agent: true,
            opener: Some(1),
        };
        // The queue held, the page is known before its `Create` is queued.
        let (outbox, shared) = (host.outbox.clone(), host.shared.clone());
        let held = outbox.0.lock().unwrap();
        let sender = std::thread::spawn(move || {
            assert!(host.send(&create(7)));
            host
        });
        let until = Instant::now() + Duration::from_secs(5);
        while !shared.0.lock().unwrap().made.contains(&7) {
            assert!(Instant::now() < until, "made only once queued");
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(held);
        apply_event(&shared, Event::Closed { id: 7 });
        let host = sender.join().unwrap();
        assert!(host.is_closed(7));

        host.outbox.0.lock().unwrap().broken = true;
        assert!(!host.send(&create(8)));
        assert!(!host.shared.0.lock().unwrap().made.contains(&8), "not sent");
    }

    #[test]
    fn a_forgotten_page_stays_forgotten() {
        let mut host = WebHost {
            child: None,
            outbox: Arc::default(),
            shared: Arc::default(),
            next_req: AtomicU64::new(1),
            readers: HashMap::new(),
            reaped: Arc::default(),
        };
        apply_event(&host.shared, Event::Title { id: 1, title: "t".into() });
        host.forget(1);
        apply_event(&host.shared, Event::Title { id: 1, title: "late".into() });
        assert!(host.view(1).is_none(), "late events don't bring it back");
        apply_event(&host.shared, Event::Closed { id: 1 });
        let s = host.shared.0.lock().unwrap();
        assert!(s.closed.is_empty() && s.forgotten.is_empty(), "nothing kept for it");
    }

    #[test]
    fn only_replies_to_pending_requests_are_kept() {
        let shared: (Mutex<Shared>, Condvar) = Default::default();
        shared.0.lock().unwrap().pending.insert(7, 2);
        // Unsolicited, and claiming another page's request: dropped.
        apply_event(&shared, eval_result(1, 99, "\"forged\""));
        apply_event(&shared, eval_result(1, 7, "\"pwned\""));
        assert!(shared.0.lock().unwrap().replies.is_empty());
        // The real answer still lands; a second one for the same request doesn't.
        apply_event(&shared, eval_result(2, 7, "2"));
        apply_event(&shared, eval_result(2, 7, "\"again\""));
        let s = shared.0.lock().unwrap();
        assert_eq!(s.replies.get(&7), Some(&eval_result(2, 7, "2")));
        assert!(s.pending.is_empty());
    }
}
