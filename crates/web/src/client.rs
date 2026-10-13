//! App-side handle to the web host process: spawns it, sends [`Request`]s, and
//! tracks per-browser state from the [`Event`] stream on a reader thread.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::frame::FrameReader;
use crate::protocol::{Cursor, Event, Request};

/// Executable name of the host (and of its macOS bundle, `<name>.app`).
pub const HOST_NAME: &str = "ghostrealm-web-host";

/// Set to a host executable path to override discovery.
pub const HOST_ENV: &str = "GHOSTREALM_WEB_HOST";

/// Set (to anything) to let the host's stderr through instead of discarding it.
pub const DEBUG_ENV: &str = "GHOSTREALM_WEB_DEBUG";

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
    /// Bumped on every event for this page, so a caller can tell it changed.
    pub version: u64,
}

/// A page frame the app can hold onto: its shared mapping plus where in it the
/// pixels are. Reading never copies; the host may overwrite the slot two frames
/// later, which at worst shows a newer frame.
#[derive(Clone)]
pub struct SharedFrame {
    reader: Arc<FrameReader>,
    pub slot: u8,
    pub width: u32,
    pub height: u32,
    pub seq: u64,
}

impl SharedFrame {
    /// The frame's BGRA pixels.
    pub fn pixels(&self) -> Option<&[u8]> {
        self.reader.slot(self.slot, self.width, self.height)
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
    views: HashMap<u64, ViewState>,
    /// `Text` / `Eval` requests awaiting an answer: request id → page id.
    /// Replies to anything else are dropped.
    pending: HashMap<u64, u64>,
    /// Answers to pending requests, keyed by request id.
    replies: HashMap<u64, Event>,
}

type Waker = Arc<dyn Fn() + Send + Sync>;

pub struct WebHost {
    child: Option<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    shared: Arc<(Mutex<Shared>, Condvar)>,
    next_req: AtomicU64,
    readers: HashMap<u64, Arc<FrameReader>>,
}

impl WebHost {
    /// Start the host at `exe`. `waker` runs (on the reader thread) after every
    /// event, so the GUI can redraw.
    pub fn spawn(exe: &Path, waker: Option<Waker>) -> Result<Self> {
        let stderr = if std::env::var_os(DEBUG_ENV).is_some() {
            Stdio::inherit()
        } else {
            Stdio::null()
        };
        let mut child = Command::new(exe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .with_context(|| format!("spawn web host {}", exe.display()))?;
        let stdin = child.stdin.take().context("web host stdin")?;
        let stdout = child.stdout.take().context("web host stdout")?;
        let shared: Arc<(Mutex<Shared>, Condvar)> = Arc::default();
        let reader_shared = shared.clone();
        std::thread::Builder::new()
            .name("web-host-events".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    let Ok(ev) = serde_json::from_str::<Event>(&line) else {
                        continue;
                    };
                    apply_event(&reader_shared, ev);
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
        Ok(WebHost {
            child: Some(child),
            stdin: Mutex::new(Some(stdin)),
            shared,
            next_req: AtomicU64::new(1),
            readers: HashMap::new(),
        })
    }

    /// Send a request. Returns false if the host has gone away.
    pub fn send(&self, req: &Request) -> bool {
        let Ok(mut line) = serde_json::to_vec(req) else {
            return false;
        };
        line.push(b'\n');
        let mut guard = self.stdin.lock().unwrap();
        let Some(stdin) = guard.as_mut() else {
            return false;
        };
        if stdin.write_all(&line).and_then(|_| stdin.flush()).is_err() {
            *guard = None;
            return false;
        }
        true
    }

    /// Whether the host process is still running.
    pub fn is_alive(&self) -> bool {
        !self.shared.0.lock().unwrap().exited
    }

    /// Block until the host reports CEF is initialised (or `timeout`/exit).
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        self.wait_until(timeout, |s| s.ready)
            .is_some_and(|s| s.ready)
    }

    /// A snapshot of browser `id`'s state.
    pub fn view(&self, id: u64) -> Option<ViewState> {
        self.shared.0.lock().unwrap().views.get(&id).cloned()
    }

    /// Drop local state for a closed browser.
    pub fn forget(&mut self, id: u64) {
        self.shared.0.lock().unwrap().views.remove(&id);
        self.readers.remove(&id);
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
        frame.pixels().is_some().then_some(frame)
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

    /// Evaluate `js` in the main frame. `Ok` carries the JSON-encoded value,
    /// which the page can shape (see [`eval_wrapper`]).
    ///
    /// [`eval_wrapper`]: crate::protocol::eval_wrapper
    pub fn eval(&self, id: u64, js: &str, timeout: Duration) -> Result<String, String> {
        let req = self.expect_reply(id);
        if !self.send(&Request::Eval {
            id,
            req,
            js: js.to_string(),
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
        // EOF on stdin asks the host to close its browsers and exit; reap it off
        // this thread so a slow shutdown never blocks the caller.
        self.stdin.lock().unwrap().take();
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = std::thread::Builder::new()
            .name("web-host-reap".into())
            .spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    if let Ok(Some(_)) = child.try_wait() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _ = child.kill();
                let _ = child.wait();
            });
    }
}

fn apply_event(shared: &(Mutex<Shared>, Condvar), ev: Event) {
    let (lock, cv) = shared;
    let mut s = lock.lock().unwrap();
    if let Some(id) = page_of(&ev) {
        s.views.entry(id).or_default().version += 1;
    }
    match ev {
        Event::Ready => s.ready = true,
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
        Event::Closed { id } => {
            s.views.remove(&id);
        }
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
}

/// The page an event updates the state of (not replies, which answer requests).
fn page_of(ev: &Event) -> Option<u64> {
    match ev {
        Event::Frame { id, .. }
        | Event::Address { id, .. }
        | Event::Title { id, .. }
        | Event::Loading { id, .. }
        | Event::Cursor { id, .. }
        | Event::LoadError { id, .. } => Some(*id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval_result(id: u64, req: u64, value: &str) -> Event {
        Event::EvalResult {
            id,
            req,
            ok: true,
            value: value.into(),
        }
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
